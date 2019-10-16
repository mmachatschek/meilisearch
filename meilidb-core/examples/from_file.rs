use std::collections::btree_map::{BTreeMap, Entry};
use std::collections::HashSet;
use std::error::Error;
use std::io::Write;
use std::iter::FromIterator;
use std::path::{Path, PathBuf};
use std::time::{Instant, Duration};
use std::{fs, io, sync::mpsc};

use rustyline::{Editor, Config};
use serde::{Serialize, Deserialize};
use structopt::StructOpt;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

use meilidb_core::{Highlight, Database, UpdateResult};
use meilidb_schema::SchemaAttr;

const INDEX_NAME: &str = "default";

#[derive(Debug, StructOpt)]
struct IndexCommand {
    /// The destination where the database must be created.
    #[structopt(parse(from_os_str))]
    database_path: PathBuf,

    /// The csv file to index.
    #[structopt(parse(from_os_str))]
    csv_data_path: PathBuf,

    /// The path to the schema.
    #[structopt(long, parse(from_os_str))]
    schema: PathBuf,

    #[structopt(long)]
    update_group_size: Option<usize>,
}

#[derive(Debug, StructOpt)]
struct SearchCommand {
    /// The destination where the database must be created.
    #[structopt(parse(from_os_str))]
    database_path: PathBuf,

    /// Timeout after which the search will return results.
    #[structopt(long)]
    fetch_timeout_ms: Option<u64>,

    /// The number of returned results
    #[structopt(short, long, default_value = "10")]
    number_results: usize,

    /// The number of characters before and after the first match
    #[structopt(short = "C", long, default_value = "35")]
    char_context: usize,

    /// A filter string that can be `!adult` or `adult` to
    /// filter documents on this specfied field
    #[structopt(short, long)]
    filter: Option<String>,

    /// Fields that must be displayed.
    displayed_fields: Vec<String>,
}

#[derive(Debug, StructOpt)]
enum Command {
    Index(IndexCommand),
    Search(SearchCommand),
}

impl Command {
    fn path(&self) -> &Path {
        match self {
            Command::Index(command) => &command.database_path,
            Command::Search(command) => &command.database_path,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct Document(indexmap::IndexMap<String, String>);

fn index_command(command: IndexCommand, database: Database) -> Result<(), Box<dyn Error>> {
    let start = Instant::now();

    let (sender, receiver) = mpsc::sync_channel(100);
    let update_fn = move |update: UpdateResult| sender.send(update.update_id).unwrap();
    let index = match database.open_index(INDEX_NAME) {
        Some(index) => index,
        None => database.create_index(INDEX_NAME).unwrap()
    };

    let done = database.set_update_callback(INDEX_NAME, Box::new(update_fn));
    assert!(done, "could not set the index update function");

    let env = &database.env;

    let schema = {
        let string = fs::read_to_string(&command.schema)?;
        toml::from_str(&string).unwrap()
    };

    let mut writer = env.write_txn().unwrap();
    match index.main.schema(&writer)? {
        Some(current_schema) => {
            if current_schema != schema {
                return Err(meilidb_core::Error::SchemaDiffer.into())
            }
            writer.abort();
        },
        None => {
            index.schema_update(&mut writer, schema)?;
            writer.commit().unwrap();
        },
    }

    let mut rdr = csv::Reader::from_path(command.csv_data_path)?;
    let mut raw_record = csv::StringRecord::new();
    let headers = rdr.headers()?.clone();

    let mut max_update_id = 0;
    let mut i = 0;
    let mut end_of_file = false;

    while !end_of_file {
        let mut additions = index.documents_addition();

        loop {
            end_of_file = !rdr.read_record(&mut raw_record)?;
            if end_of_file { break }

            let document: Document = match raw_record.deserialize(Some(&headers)) {
                Ok(document) => document,
                Err(e) => {
                    eprintln!("{:?}", e);
                    continue;
                }
            };

            additions.update_document(document);

            print!("\rindexing document {}", i);
            i += 1;

            if let Some(group_size) = command.update_group_size {
                if i % group_size == 0 { break }
            }
        }

        println!();

        let mut writer = env.write_txn().unwrap();
        println!("committing update...");
        let update_id = additions.finalize(&mut writer)?;
        writer.commit().unwrap();
        max_update_id = max_update_id.max(update_id);
        println!("committed update {}", update_id);
    }

    println!("Waiting for update {}", max_update_id);
    for id in receiver {
        if id == max_update_id { break }
    }

    println!("database created in {:.2?} at: {:?}", start.elapsed(), command.database_path);

    Ok(())
}

fn display_highlights(text: &str, ranges: &[usize]) -> io::Result<()> {
    let mut stdout = StandardStream::stdout(ColorChoice::Always);
    let mut highlighted = false;

    for range in ranges.windows(2) {
        let [start, end] = match range { [start, end] => [*start, *end], _ => unreachable!() };
        if highlighted {
            stdout.set_color(ColorSpec::new().set_fg(Some(Color::Yellow)))?;
        }
        write!(&mut stdout, "{}", &text[start..end])?;
        stdout.reset()?;
        highlighted = !highlighted;
    }

    Ok(())
}

fn char_to_byte_range(index: usize, length: usize, text: &str) -> (usize, usize) {
    let mut byte_index = 0;
    let mut byte_length = 0;

    for (n, (i, c)) in text.char_indices().enumerate() {
        if n == index {
            byte_index = i;
        }

        if n + 1 == index + length {
            byte_length = i - byte_index + c.len_utf8();
            break;
        }
    }

    (byte_index, byte_length)
}

fn create_highlight_areas(text: &str, highlights: &[Highlight]) -> Vec<usize> {
    let mut byte_indexes = BTreeMap::new();

    for highlight in highlights {
        let char_index = highlight.char_index as usize;
        let char_length = highlight.char_length as usize;
        let (byte_index, byte_length) = char_to_byte_range(char_index, char_length, text);

        match byte_indexes.entry(byte_index) {
            Entry::Vacant(entry) => { entry.insert(byte_length); },
            Entry::Occupied(mut entry) => {
                if *entry.get() < byte_length {
                    entry.insert(byte_length);
                }
            },
        }
    }

    let mut title_areas = Vec::new();
    title_areas.push(0);
    for (byte_index, length) in byte_indexes {
        title_areas.push(byte_index);
        title_areas.push(byte_index + length);
    }
    title_areas.push(text.len());
    title_areas.sort_unstable();
    title_areas
}

/// note: matches must have been sorted by `char_index` and `char_length` before being passed.
///
/// ```no_run
/// matches.sort_unstable_by_key(|m| (m.char_index, m.char_length));
///
/// let matches = matches.matches.iter().filter(|m| SchemaAttr::new(m.attribute) == attr).cloned();
///
/// let (text, matches) = crop_text(&text, matches, 35);
/// ```
fn crop_text(
    text: &str,
    highlights: impl IntoIterator<Item=Highlight>,
    context: usize,
) -> (String, Vec<Highlight>)
{
    let mut highlights = highlights.into_iter().peekable();

    let char_index = highlights.peek().map(|m| m.char_index as usize).unwrap_or(0);
    let start = char_index.saturating_sub(context);
    let text = text.chars().skip(start).take(context * 2).collect();

    let highlights = highlights
        .take_while(|m| {
            (m.char_index as usize) + (m.char_length as usize) <= start + (context * 2)
        })
        .map(|highlight| {
            Highlight { char_index: highlight.char_index - start as u16, ..highlight }
        })
        .collect();

    (text, highlights)
}

fn search_command(command: SearchCommand, database: Database) -> Result<(), Box<dyn Error>> {
    let env = &database.env;
    let index = database.open_index(INDEX_NAME).expect("Could not find index");
    let reader = env.read_txn().unwrap();

    let schema = index.main.schema(&reader)?;
    let schema = schema.ok_or(meilidb_core::Error::SchemaMissing)?;

    let fields = command.displayed_fields.iter().map(String::as_str);
    let fields = HashSet::from_iter(fields);

    let config = Config::builder().auto_add_history(true).build();
    let mut readline = Editor::<()>::with_config(config);
    let _ = readline.load_history("query-history.txt");

    for result in readline.iter("Searching for: ") {
        match result {
            Ok(query) => {
                let start_total = Instant::now();

                let documents = match command.filter {
                    Some(ref filter) => {
                        let filter = filter.as_str();
                        let (positive, filter) = if filter.chars().next() == Some('!') {
                            (false, &filter[1..])
                        } else {
                            (true, filter)
                        };

                        let attr = schema.attribute(&filter).expect("Could not find filtered attribute");

                        let builder = index.query_builder();
                        let builder = builder.with_filter(|document_id| {
                            let string: String = index.document_attribute(&reader, document_id, attr).unwrap().unwrap();
                            (string == "true") == positive
                        });
                        builder.query(&reader, &query, 0..command.number_results)?
                    },
                    None => {
                        let builder = index.query_builder();
                        builder.query(&reader, &query, 0..command.number_results)?
                    }
                };

                let mut retrieve_duration = Duration::default();

                let number_of_documents = documents.len();
                for mut doc in documents {

                    doc.highlights.sort_unstable_by_key(|m| (m.char_index, m.char_length));

                    let start_retrieve = Instant::now();
                    let result = index.document::<Document>(&reader, Some(&fields), doc.id);
                    retrieve_duration += start_retrieve.elapsed();

                    match result {
                        Ok(Some(document)) => {
                            println!("raw-id: {:?}", doc.id);
                            for (name, text) in document.0 {
                                print!("{}: ", name);

                                let attr = schema.attribute(&name).unwrap();
                                let highlights = doc.highlights.iter()
                                                .filter(|m| SchemaAttr::new(m.attribute) == attr)
                                                .cloned();
                                let (text, highlights) = crop_text(&text, highlights, command.char_context);
                                let areas = create_highlight_areas(&text, &highlights);
                                display_highlights(&text, &areas)?;
                                println!();
                            }
                        },
                        Ok(None) => eprintln!("missing document"),
                        Err(e) => eprintln!("{}", e),
                    }

                    let mut matching_attributes = HashSet::new();
                    for highlight in doc.highlights {
                        let attr = SchemaAttr::new(highlight.attribute);
                        let name = schema.attribute_name(attr);
                        matching_attributes.insert(name);
                    }

                    let matching_attributes = Vec::from_iter(matching_attributes);
                    println!("matching in: {:?}", matching_attributes);

                    println!();
                }

                eprintln!("whole documents fields retrieve took {:.2?}", retrieve_duration);
                eprintln!("===== Found {} results in {:.2?} =====", number_of_documents, start_total.elapsed());
            },
            Err(err) => {
                println!("Error: {:?}", err);
                break
            }
        }
    }

    readline.save_history("query-history.txt").unwrap();

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let opt = Command::from_args();
    let database = Database::open_or_create(opt.path())?;

    match opt {
        Command::Index(command) => index_command(command, database),
        Command::Search(command) => search_command(command, database),
    }
}