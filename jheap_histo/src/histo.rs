//! Holds models for JMap histograms and logic for parsing
//! them out of files

use combine::easy;

use chrono::{DateTime, Local, Utc};
use combine::error::UnexpectedParse;
use combine::parser::char::*;
use combine::*;
use std::num::ParseIntError;
use std::path::Path;
use thiserror::*;

#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub rank: usize,
    pub instances_count: usize,
    pub bytes: usize,
    pub class_name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Histogram(pub Vec<Entry>);

#[derive(Debug, PartialEq, Eq)]
pub struct HistogramWithTimestamp {
    pub timestamp: DateTime<Utc>,
    pub histogram: Histogram,
}

impl HistogramWithTimestamp {
    /// Given a Path, parses it into a HistogramWithTimestamp
    pub async fn from_path<P: AsRef<Path>>(
        p: P,
    ) -> Result<HistogramWithTimestamp, HistogramWithTimestampFromFileError> {
        debug!("Parsing file at [{:?}]", p.as_ref());
        let file = tokio::fs::File::open(&p).await?;
        let contents = tokio::fs::read_to_string(&p).await?;
        let file_metadata = file.metadata().await?;
        let timestamp = {
            let filename_as_datetime_or_err = p
                .as_ref()
                .file_name()
                .and_then(|os_str| os_str.to_str())
                .ok_or(HistogramWithTimestampFromFileError::FilenameNotStringError)
                .and_then(|str| {
                    let t = DateTime::parse_from_rfc3339(str)?;
                    debug!("Using date from file name [{:?}]", t);
                    Ok(t.with_timezone(&Utc))
                });

            filename_as_datetime_or_err.or_else::<HistogramWithTimestampFromFileError, _>(|_| {
                let t: DateTime<Local> = file_metadata
                    .created()
                    .or_else(|_| file_metadata.modified())?
                    .into();
                debug!("Using date from file metadata [{:?}]", t);
                Ok(t.with_timezone(&Utc))
            })?
        };
        let histogram = Histogram::parse(&contents)?;
        Ok(HistogramWithTimestamp {
            timestamp,
            histogram,
        })
    }
}

impl Histogram {
    /// Given an str, parses it into a Histogram
    pub fn parse(s: &str) -> Result<Histogram, HistogramParseError> {
        let mut parser = header_parser()
            .with(many(entry_parser().skip(spaces())))
            .map(Histogram);
        let (result, remainder) = parser.easy_parse(s)?;
        debug!("Successfully parsed histogram, remaining [{}]", remainder);
        Ok(result)
    }
}

#[derive(Error, Debug)]
#[error("Failed to parse histogram")]
pub struct HistogramParseError(UnexpectedParse);

impl<'a> From<easy::ParseError<&'a str>> for HistogramParseError {
    fn from(e: easy::ParseError<&'a str>) -> Self {
        HistogramParseError(e.into_other())
    }
}

#[derive(Error, Debug)]
pub enum HistogramWithTimestampFromFileError {
    #[error("Failed to read Histogram data")]
    IoError(#[from] std::io::Error),
    #[error("Failed to parse Histogram data")]
    ParseError(#[from] HistogramParseError),
    #[error("Filename not useable as String")]
    FilenameNotStringError,
    #[error("Filename not parseable as DateTime")]
    FilenameNotDateTimeError(#[from] chrono::ParseError),
}

fn entry_parser<Input>() -> impl Parser<Input, Output = Entry>
where
    Input: Stream<Token = char>,
    Input::Error: ParseError<Input::Token, Input::Range, Input::Position>,
    <<Input as StreamOnce>::Error as combine::ParseError<
        char,
        <Input as StreamOnce>::Range,
        <Input as StreamOnce>::Position,
    >>::StreamError: From<ParseIntError>,
    <<Input as StreamOnce>::Error as combine::ParseError<
        char,
        <Input as StreamOnce>::Range,
        <Input as StreamOnce>::Position,
    >>::StreamError: From<ParseIntError>,
    <Input as combine::StreamOnce>::Error: combine::ParseError<
        char,
        <Input as combine::StreamOnce>::Range,
        <Input as combine::StreamOnce>::Position,
    >,
{
    spaces()
        .with(number_parser())
        .skip(char(':'))
        .skip(spaces())
        .and(number_parser())
        .skip(spaces())
        .and(number_parser())
        .skip(spaces())
        .and(many::<String, _, _>(
            alpha_num().or(one_of("!\"#$%&'()=~|-^¥@[;:],./_`{+*}<>?_ ".chars())),
        ))
        .map(|(((rank, instances_count), bytes), class_name)| Entry {
            rank,
            instances_count,
            bytes,
            class_name,
        })
}

fn header_parser<Input>() -> impl Parser<Input, Output = ()>
where
    Input: Stream<Token = char>,
    Input::Error: ParseError<Input::Token, Input::Range, Input::Position>,
{
    spaces()
        .with(string("num"))
        .skip(spaces())
        .skip(string("#instances"))
        .skip(spaces())
        .skip(string("#bytes"))
        .skip(spaces())
        .skip(string("class name"))
        .skip(spaces())
        .skip(optional(string("(module)")))
        .skip(spaces())
        .skip(many::<String, _, _>(char('-')))
        .map(|_| ())
}

fn number_parser<Input>() -> impl Parser<Input, Output = usize>
where
    Input: Stream<Token = char>,
    Input::Error: ParseError<Input::Token, Input::Range, Input::Position>,
    <<Input as StreamOnce>::Error as combine::ParseError<
        char,
        <Input as StreamOnce>::Range,
        <Input as StreamOnce>::Position,
    >>::StreamError: From<ParseIntError>,
    <<Input as StreamOnce>::Error as combine::ParseError<
        char,
        <Input as StreamOnce>::Range,
        <Input as StreamOnce>::Position,
    >>::StreamError: From<ParseIntError>,
    <Input as combine::StreamOnce>::Error: combine::ParseError<
        char,
        <Input as combine::StreamOnce>::Range,
        <Input as combine::StreamOnce>::Position,
    >,
{
    many::<String, _, _>(digit()).and_then(|d| d.parse::<usize>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    static SAMPLE_HISTO_WITH_HEADER: &str = include_str!("../test/data/sample_histo_with_header");
    static SAMPLE_HISTO_WITH_MODULE_HEADER: &str =
        include_str!("../test/data/sample_histo_with_module_header");

    #[test]
    fn parse_entry_test() {
        let input =
            "  14:          7999        1290520  [Ljava.util.concurrent.ConcurrentHashMap$Node;";
        let mut parser = entry_parser();
        let r = parser.easy_parse(input).unwrap().0;
        let expected = Entry {
            rank: 14,
            instances_count: 7999,
            bytes: 1290520,
            class_name: "[Ljava.util.concurrent.ConcurrentHashMap$Node;".to_string(),
        };
        assert_eq!(expected, r);
    }

    #[test]
    fn parse_with_header_test() {
        let r = Histogram::parse(SAMPLE_HISTO_WITH_HEADER).unwrap();
        assert_eq!(40, r.0.len())
    }

    #[test]
    fn parse_with_module_header_test() {
        let r = Histogram::parse(SAMPLE_HISTO_WITH_MODULE_HEADER).unwrap();
        assert_eq!(13831, r.0.len())
    }

    #[tokio::test]
    async fn open_histogram_with_timestamp_test() {
        let histo_file_path = {
            let mut f = PathBuf::from(file!());
            f.pop();
            f.pop();
            f.pop();
            f.push("test/data/sample_histo_with_header");
            f
        };
        let r = HistogramWithTimestamp::from_path(&histo_file_path)
            .await
            .unwrap();

        assert_eq!(40, r.histogram.0.len())
    }
}
