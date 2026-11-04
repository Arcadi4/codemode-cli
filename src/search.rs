use clap::Args;
use serde_json::{Map, Value};

use crate::host::CatalogEntry;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Field {
    Name,
    Server,
    Title,
    Description,
}

#[derive(Args)]
pub struct SearchArgs {
    /// Whitespace-separated terms; every term must match somewhere (case-insensitive)
    #[arg(value_parser = parse_query)]
    pub query: String,
    /// Maximum number of matches to return
    #[arg(long, default_value = "20", value_parser = clap::value_parser!(u64).range(1..))]
    pub limit: u64,
    /// Summary fields to include in each match
    #[arg(
        long,
        value_delimiter = ',',
        value_enum,
        default_value = "name,server,description"
    )]
    pub fields: Vec<Field>,
}

fn parse_query(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("search query must not be empty".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Exact,
    Prefix,
    Substring,
    Metadata,
}

pub fn search(entries: Vec<CatalogEntry>, args: &SearchArgs) -> Value {
    let query_lower = args.query.to_lowercase();
    let terms: Vec<&str> = query_lower.split_whitespace().collect();
    if terms.is_empty() {
        return Value::Array(Vec::new());
    }
    let normalized_query = terms.join(" ");
    let mut matches: Vec<(Rank, CatalogEntry)> = entries
        .into_iter()
        .filter_map(|entry| rank(&entry, &terms, &normalized_query).map(|rank| (rank, entry)))
        .collect();
    matches.sort_by(|(left_rank, left), (right_rank, right)| {
        left_rank
            .cmp(right_rank)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.server.cmp(&right.server))
    });
    matches.truncate(usize::try_from(args.limit).unwrap_or(usize::MAX));
    Value::Array(
        matches
            .into_iter()
            .map(|(_, entry)| project(entry, &args.fields))
            .collect(),
    )
}

fn rank(entry: &CatalogEntry, terms: &[&str], query_lower: &str) -> Option<Rank> {
    let name_lower = entry.name.to_lowercase();
    let native_lower = entry.native_name().to_lowercase();
    let all_in_native = terms.iter().all(|&term| native_lower.contains(term));
    let all_in_name = all_in_native || terms.iter().all(|&term| name_lower.contains(term));
    let (title_lower, description_lower) = if all_in_name {
        (None, None)
    } else {
        (
            lower_text(&entry.definition, "title"),
            lower_text(&entry.definition, "description"),
        )
    };
    for &term in terms {
        let found = name_lower.contains(term)
            || native_lower.contains(term)
            || title_lower
                .as_deref()
                .is_some_and(|title| title.contains(term))
            || description_lower
                .as_deref()
                .is_some_and(|description| description.contains(term));
        if !found {
            return None;
        }
    }
    Some(
        if native_lower == query_lower || name_lower == query_lower {
            Rank::Exact
        } else if native_lower.starts_with(query_lower)
            || name_lower.starts_with(query_lower)
            || (all_in_native && native_lower.starts_with(terms[0]))
            || (all_in_name && name_lower.starts_with(terms[0]))
        {
            Rank::Prefix
        } else if all_in_name {
            Rank::Substring
        } else {
            Rank::Metadata
        },
    )
}

fn lower_text(definition: &Value, key: &str) -> Option<String> {
    definition
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_lowercase)
}

fn project(mut entry: CatalogEntry, fields: &[Field]) -> Value {
    let mut object = Map::new();
    for field in [Field::Name, Field::Server, Field::Title, Field::Description] {
        if !fields.contains(&field) {
            continue;
        }
        match field {
            Field::Name => {
                object.insert(
                    "name".to_owned(),
                    Value::String(std::mem::take(&mut entry.name)),
                );
            }
            Field::Server => {
                object.insert(
                    "server".to_owned(),
                    Value::String(std::mem::take(&mut entry.server)),
                );
            }
            Field::Title | Field::Description => {
                let key = if field == Field::Title {
                    "title"
                } else {
                    "description"
                };
                if let Some(Value::String(text)) = entry.definition.get_mut(key) {
                    object.insert(key.to_owned(), Value::String(std::mem::take(text)));
                }
            }
        }
    }
    Value::Object(object)
}
