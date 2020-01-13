use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::time::Instant;
use std::{cmp, fmt, iter::once};

use sdset::{Set, SetBuf, SetOperation};
use slice_group_by::StrGroupBy;
use itertools::{EitherOrBoth, merge_join_by};
use fst::{IntoStreamer, Streamer};

use crate::database::MainT;
use crate::{store, DocumentId, DocIndex, MResult};
use crate::automaton::{build_dfa, build_prefix_dfa, build_exact_dfa};
use crate::QueryWordsMapper;

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Operation {
    And(Vec<Operation>),
    Or(Vec<Operation>),
    Query(Query),
}

impl fmt::Debug for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn pprint_tree(f: &mut fmt::Formatter<'_>, op: &Operation, depth: usize) -> fmt::Result {
            match op {
                Operation::And(children) => {
                    writeln!(f, "{:1$}AND", "", depth * 2)?;
                    children.iter().try_for_each(|c| pprint_tree(f, c, depth + 1))
                },
                Operation::Or(children) => {
                    writeln!(f, "{:1$}OR", "", depth * 2)?;
                    children.iter().try_for_each(|c| pprint_tree(f, c, depth + 1))
                },
                Operation::Query(query) => writeln!(f, "{:2$}{:?}", "", query, depth * 2),
            }
        }

        pprint_tree(f, self, 0)
    }
}

impl Operation {
    fn tolerant(id: QueryId, prefix: bool, s: &str) -> Operation {
        Operation::Query(Query { id, prefix, kind: QueryKind::Tolerant(s.to_string()) })
    }

    fn exact(id: QueryId, prefix: bool, s: &str) -> Operation {
        Operation::Query(Query { id, prefix, kind: QueryKind::Exact(s.to_string()) })
    }

    fn phrase2(id: QueryId, prefix: bool, (left, right): (&str, &str)) -> Operation {
        Operation::Query(Query { id, prefix, kind: QueryKind::Phrase(vec![left.to_owned(), right.to_owned()]) })
    }
}

pub type QueryId = usize;

#[derive(Clone, Eq)]
pub struct Query {
    pub id: QueryId,
    pub prefix: bool,
    pub kind: QueryKind,
}

impl PartialEq for Query {
    fn eq(&self, other: &Self) -> bool {
        self.prefix == other.prefix && self.kind == other.kind
    }
}

impl Hash for Query {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.prefix.hash(state);
        self.kind.hash(state);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum QueryKind {
    Tolerant(String),
    Exact(String),
    Phrase(Vec<String>),
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Query { id, prefix, kind } = self;
        let prefix = if *prefix { String::from("Prefix") } else { String::default() };
        match kind {
            QueryKind::Exact(word) => {
                f.debug_struct(&(prefix + "Exact")).field("id", &id).field("word", &word).finish()
            },
            QueryKind::Tolerant(word) => {
                f.debug_struct(&(prefix + "Tolerant")).field("id", &id).field("word", &word).finish()
            },
            QueryKind::Phrase(words) => {
                f.debug_struct(&(prefix + "Phrase")).field("id", &id).field("words", &words).finish()
            },
        }
    }
}

#[derive(Debug, Default)]
pub struct PostingsList {
    docids: SetBuf<DocumentId>,
    matches: SetBuf<DocIndex>,
}

pub struct Context {
    pub words_set: fst::Set,
    pub synonyms: store::Synonyms,
    pub postings_lists: store::PostingsLists,
    pub prefix_postings_lists: store::PrefixPostingsListsCache,
}

fn split_best_frequency<'a>(reader: &heed::RoTxn<MainT>, ctx: &Context, word: &'a str) -> MResult<Option<(&'a str, &'a str)>> {
    let chars = word.char_indices().skip(1);
    let mut best = None;

    for (i, _) in chars {
        let (left, right) = word.split_at(i);

        let left_freq = ctx.postings_lists
            .postings_list(reader, left.as_bytes())?
            .map(|p| p.docids.len())
            .unwrap_or(0);
        let right_freq = ctx.postings_lists
            .postings_list(reader, right.as_bytes())?
            .map(|p| p.docids.len())
            .unwrap_or(0);

        let min_freq = cmp::min(left_freq, right_freq);
        if min_freq != 0 && best.map_or(true, |(old, _, _)| min_freq > old) {
            best = Some((min_freq, left, right));
        }
    }

    Ok(best.map(|(_, l, r)| (l, r)))
}

fn fetch_synonyms(reader: &heed::RoTxn<MainT>, ctx: &Context, words: &[&str]) -> MResult<Vec<Vec<String>>> {
    let words = words.join(" "); // TODO ugly
    // synonyms.synonyms(reader, words.as_bytes()).cloned().unwrap_or_default()
    Ok(vec![])
}

fn is_last<I: IntoIterator>(iter: I) -> impl Iterator<Item=(bool, I::Item)> {
    let mut iter = iter.into_iter().peekable();
    core::iter::from_fn(move || {
        iter.next().map(|item| (iter.peek().is_none(), item))
    })
}

fn create_operation<I, F>(iter: I, f: F) -> Operation
where I: IntoIterator<Item=Operation>,
      F: Fn(Vec<Operation>) -> Operation,
{
    let mut iter = iter.into_iter();
    match (iter.next(), iter.next()) {
        (Some(first), None) => first,
        (first, second) => f(first.into_iter().chain(second).chain(iter).collect()),
    }
}

const MAX_NGRAM: usize = 3;

pub fn create_query_tree(
    reader: &heed::RoTxn<MainT>,
    ctx: &Context,
    query: &str,
) -> MResult<(Operation, HashMap<QueryId, Range<usize>>)>
{
    let query = query.to_lowercase();
    let words = query.linear_group_by_key(char::is_whitespace).map(ToOwned::to_owned);
    let words: Vec<_> = words.filter(|s| !s.contains(char::is_whitespace)).enumerate().collect();

    let mut mapper = QueryWordsMapper::new(words.iter().map(|(_, w)| w));
    let mut ngrams = Vec::new();
    for ngram in 1..=MAX_NGRAM {

        let ngiter = words.windows(ngram).enumerate().map(|(i, group)| {
            let before = words[0..i].windows(1).enumerate().map(|(i, g)| (i..i+1, g));
            let after = words[i + ngram..].windows(1)
                .enumerate()
                .map(move |(j, g)| (i + j + ngram..i + j + ngram + 1, g));
            before.chain(Some((i..i + ngram, group))).chain(after)
        });

        for group in ngiter {

            let mut ops = Vec::new();
            for (is_last, (range, words)) in is_last(group) {

                let mut alts = Vec::new();
                match words {
                    [(id, word)] => {
                        let mut idgen = ((id + 1) * 100)..;

                        let phrase = split_best_frequency(reader, ctx, word)?
                            .map(|ws| {
                                let id = idgen.next().unwrap();
                                idgen.next().unwrap();
                                mapper.declare(range.clone(), id, &[ws.0, ws.1]);
                                Operation::phrase2(id, is_last, ws)
                            });

                        let synonyms = fetch_synonyms(reader, ctx, &[word])?
                            .into_iter()
                            .map(|alts| {
                                let id = idgen.next().unwrap();
                                mapper.declare(range.clone(), id, &alts);

                                let mut idgen = once(id).chain(&mut idgen);
                                let iter = alts.into_iter().map(|w| {
                                    let id = idgen.next().unwrap();
                                    Operation::exact(id, false, &w)
                                });

                                create_operation(iter, Operation::And)
                            });

                        let query = Operation::tolerant(*id, is_last, word);

                        alts.push(query);
                        alts.extend(synonyms.chain(phrase));
                    },
                    words => {
                        let id = words[0].0;
                        let mut idgen = ((id + 1) * 100_usize.pow(ngram as u32))..;

                        let words: Vec<_> = words.iter().map(|(_, s)| s.as_str()).collect();

                        for synonym in fetch_synonyms(reader, ctx, &words)? {
                            let id = idgen.next().unwrap();
                            mapper.declare(range.clone(), id, &synonym);

                            let mut idgen = once(id).chain(&mut idgen);
                            let synonym = synonym.into_iter().map(|s| {
                                let id = idgen.next().unwrap();
                                Operation::exact(id, false, &s)
                            });
                            alts.push(create_operation(synonym, Operation::And));
                        }

                        let id = idgen.next().unwrap();
                        let concat = words.concat();
                        alts.push(Operation::exact(id, is_last, &concat));
                        mapper.declare(range.clone(), id, &[concat]);
                    }
                }

                ops.push(create_operation(alts, Operation::Or));
            }

            ngrams.push(create_operation(ops, Operation::And));
            if ngram == 1 { break }
        }
    }

    let mapping = mapper.mapping();
    let operation = create_operation(ngrams, Operation::Or);

    Ok((operation, mapping))
}

pub type Postings<'o, 'txn> = HashMap<(&'o Query, Vec<u8>), Cow<'txn, Set<DocIndex>>>;
pub type Cache<'o, 'txn> = HashMap<&'o Operation, Cow<'txn, Set<DocumentId>>>;

pub struct QueryResult<'o, 'txn> {
    pub docids: Cow<'txn, Set<DocumentId>>,
    pub queries: Postings<'o, 'txn>,
}

pub fn traverse_query_tree<'o, 'txn>(
    reader: &'txn heed::RoTxn<MainT>,
    ctx: &Context,
    tree: &'o Operation,
) -> MResult<QueryResult<'o, 'txn>>
{
    fn execute_and<'o, 'txn>(
        reader: &'txn heed::RoTxn<MainT>,
        ctx: &Context,
        cache: &mut Cache<'o, 'txn>,
        postings: &mut Postings<'o, 'txn>,
        depth: usize,
        operations: &'o [Operation],
    ) -> MResult<Cow<'txn, Set<DocumentId>>>
    {
        println!("{:1$}AND", "", depth * 2);

        let before = Instant::now();
        let mut results = Vec::new();

        for op in operations {
            if cache.get(op).is_none() {
                let docids = match op {
                    Operation::And(ops) => execute_and(reader, ctx, cache, postings, depth + 1, &ops)?,
                    Operation::Or(ops) => execute_or(reader, ctx, cache, postings, depth + 1, &ops)?,
                    Operation::Query(query) => execute_query(reader, ctx, postings, depth + 1, &query)?,
                };
                cache.insert(op, docids);
            }
        }

        for op in operations {
            if let Some(docids) = cache.get(op) {
                results.push(docids.as_ref());
            }
        }

        let op = sdset::multi::Intersection::new(results);
        let docids = op.into_set_buf();

        println!("{:3$}--- AND fetched {} documents in {:.02?}", "", docids.len(), before.elapsed(), depth * 2);

        Ok(Cow::Owned(docids))
    }

    fn execute_or<'o, 'txn>(
        reader: &'txn heed::RoTxn<MainT>,
        ctx: &Context,
        cache: &mut Cache<'o, 'txn>,
        postings: &mut Postings<'o, 'txn>,
        depth: usize,
        operations: &'o [Operation],
    ) -> MResult<Cow<'txn, Set<DocumentId>>>
    {
        println!("{:1$}OR", "", depth * 2);

        let before = Instant::now();
        let mut ids = Vec::new();

        for op in operations {
            let docids = match cache.get(op) {
                Some(docids) => docids,
                None => {
                    let docids = match op {
                        Operation::And(ops) => execute_and(reader, ctx, cache, postings, depth + 1, &ops)?,
                        Operation::Or(ops) => execute_or(reader, ctx, cache, postings, depth + 1, &ops)?,
                        Operation::Query(query) => execute_query(reader, ctx, postings, depth + 1, &query)?,
                    };
                    cache.entry(op).or_insert(docids)
                }
            };

            ids.extend_from_slice(docids.as_ref());
        }

        let docids = SetBuf::from_dirty(ids);

        println!("{:3$}--- OR fetched {} documents in {:.02?}", "", docids.len(), before.elapsed(), depth * 2);

        Ok(Cow::Owned(docids))
    }

    fn execute_query<'o, 'txn>(
        reader: &'txn heed::RoTxn<MainT>,
        ctx: &Context,
        postings: &mut Postings<'o, 'txn>,
        depth: usize,
        query: &'o Query,
    ) -> MResult<Cow<'txn, Set<DocumentId>>>
    {
        let before = Instant::now();

        let Query { id, prefix, kind } = query;
        let docids = match kind {
            QueryKind::Tolerant(word) => {
                if *prefix && word.len() == 1 {
                    let prefix = [word.as_bytes()[0], 0, 0, 0];
                    let result = ctx.prefix_postings_lists.prefix_postings_list(reader, prefix)?.unwrap_or_default();
                    postings.insert((query, word.clone().into_bytes()), result.matches);
                    result.docids
                } else {
                    let dfa = if *prefix { build_prefix_dfa(word) } else { build_dfa(word) };

                    let byte = word.as_bytes()[0];
                    let mut stream = if byte == u8::max_value() {
                        ctx.words_set.search(&dfa).ge(&[byte]).into_stream()
                    } else {
                        ctx.words_set.search(&dfa).ge(&[byte]).lt(&[byte + 1]).into_stream()
                    };

                    let before = Instant::now();
                    let mut docids = Vec::new();
                    while let Some(input) = stream.next() {
                        if let Some(result) = ctx.postings_lists.postings_list(reader, input)? {
                            docids.extend_from_slice(&result.docids);
                            postings.insert((query, input.to_owned()), result.matches);
                        }
                    }
                    println!("{:3$}docids extend ({:?}) took {:.02?}", "", docids.len(), before.elapsed(), depth * 2);

                    let before = Instant::now();
                    let docids = SetBuf::from_dirty(docids);
                    println!("{:2$}docids construction took {:.02?}", "", before.elapsed(), depth * 2);

                    Cow::Owned(docids)
                }
            },
            QueryKind::Exact(word) => {
                // TODO support prefix and non-prefix exact DFA
                let dfa = build_exact_dfa(word);

                let byte = word.as_bytes()[0];
                let mut stream = if byte == u8::max_value() {
                    ctx.words_set.search(&dfa).ge(&[byte]).into_stream()
                } else {
                    ctx.words_set.search(&dfa).ge(&[byte]).lt(&[byte + 1]).into_stream()
                };

                let mut docids = Vec::new();
                while let Some(input) = stream.next() {
                    if let Some(result) = ctx.postings_lists.postings_list(reader, input)? {
                        docids.extend_from_slice(&result.docids);
                        postings.insert((query, input.to_owned()), result.matches);
                    }
                }

                Cow::Owned(SetBuf::from_dirty(docids))
            },
            QueryKind::Phrase(words) => {
                // TODO support prefix and non-prefix exact DFA
                if let [first, second] = words.as_slice() {
                    let first = ctx.postings_lists.postings_list(reader, first.as_bytes())?.unwrap_or_default();
                    let second = ctx.postings_lists.postings_list(reader, second.as_bytes())?.unwrap_or_default();

                    let iter = merge_join_by(first.matches.as_slice(), second.matches.as_slice(), |a, b| {
                        let x = (a.document_id, a.attribute, (a.word_index as u32) + 1);
                        let y = (b.document_id, b.attribute, b.word_index as u32);
                        x.cmp(&y)
                    });

                    let matches: Vec<_> = iter
                        .filter_map(EitherOrBoth::both)
                        .flat_map(|(a, b)| once(*a).chain(Some(*b)))
                        .collect();

                    let before = Instant::now();
                    let mut docids: Vec<_> = matches.iter().map(|m| m.document_id).collect();
                    docids.dedup();
                    let docids = SetBuf::new(docids).unwrap();
                    println!("{:2$}docids construction took {:.02?}", "", before.elapsed(), depth * 2);

                    let matches = Cow::Owned(SetBuf::new(matches).unwrap());
                    postings.insert((query, vec![]), matches);

                    Cow::Owned(docids)
                } else {
                    println!("{:2$}{:?} skipped", "", words, depth * 2);
                    Cow::default()
                }
            },
        };

        println!("{:4$}{:?} fetched {:?} documents in {:.02?}", "", query, docids.len(), before.elapsed(), depth * 2);
        Ok(docids)
    }

    let mut cache = Cache::new();
    let mut postings = Postings::new();

    let docids = match tree {
        Operation::And(ops) => execute_and(reader, ctx, &mut cache, &mut postings, 0, &ops)?,
        Operation::Or(ops) => execute_or(reader, ctx, &mut cache, &mut postings, 0, &ops)?,
        Operation::Query(query) => execute_query(reader, ctx, &mut postings, 0, &query)?,
    };

    Ok(QueryResult { docids, queries: postings })
}
