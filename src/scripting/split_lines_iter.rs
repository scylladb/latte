use rune::runtime::Mut;
use rune::{Any, Value};
use std::fs::File;
use std::io;
use std::io::{BufRead, BufReader};

/// Iterator that reads a file line by line and splits each line using the given delimiter.
/// This provides memory-efficient processing of large files by yielding one split line at a time.
#[derive(Any, Debug)]
pub struct SplitLinesIterator {
    reader: BufReader<File>,
    delimiter: String,
    maxsplit: i64,
    do_trim: bool,
    skip_empty: bool,
}

impl SplitLinesIterator {
    pub fn new(
        path: &str,
        delimiter: &str,
        maxsplit: i64,
        do_trim: bool,
        skip_empty: bool,
    ) -> io::Result<Self> {
        let file = File::open(path)
            .map_err(|e| io::Error::new(e.kind(), format!("Failed to open file {path}: {e}")));
        match file {
            Ok(file) => {
                let reader = BufReader::new(file);
                Ok(SplitLinesIterator {
                    reader,
                    delimiter: delimiter.to_string(),
                    maxsplit,
                    do_trim,
                    skip_empty,
                })
            }
            Err(e) => {
                panic!("{}", e)
            }
        }
    }
}

impl Iterator for SplitLinesIterator {
    type Item = io::Result<Vec<String>>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None, // EOF
            Ok(_) => {
                let parts: Vec<String> = line
                    .splitn(
                        if self.maxsplit < 0 {
                            usize::MAX
                        } else {
                            (self.maxsplit + 1).try_into().unwrap()
                        },
                        &self.delimiter,
                    )
                    .map(|s| if self.do_trim { s.trim() } else { s })
                    .filter(|s| !(self.skip_empty && s.is_empty()))
                    .map(|s| s.to_string())
                    .collect();
                Some(Ok(parts))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

#[rune::function(instance)]
pub fn next(mut iter: Mut<SplitLinesIterator>) -> Option<io::Result<Vec<String>>> {
    iter.next()
}

/// Creates an iterator that reads a file line by line and splits each line using the given delimiter.
/// Returns an iterator that yields Vec<String> for each line allowing to skip empty elements.
#[rune::function]
pub fn read_split_lines_iter(
    path: &str,
    // NOTE: "params" expects following elements:
    // delimiter: &str,
    // maxsplit: i64,
    // do_trim: bool,    // per element after split
    // skip_empty: bool, // skip empty elements in a vector of a line substrings, empty vector possible
    params: Vec<Value>,
) -> io::Result<SplitLinesIterator> {
    let mut delimiter: String = " ".to_string();
    let mut maxsplit = -1;
    let mut do_trim = true;
    let mut skip_empty = true;
    match params.as_slice() {
        // (str): delimiter
        [Value::String(custom_delimiter)] => {
            delimiter = custom_delimiter.borrow_ref().unwrap().to_string();
        }
        // (int): maxsplit
        [Value::Integer(custom_maxsplit)] => {
            maxsplit = *custom_maxsplit;
        }
        // (bool): do_trim
        [Value::Bool(custom_do_trim)] => {
            do_trim = *custom_do_trim;
        }
        // (bool, bool): do_trim, skip_empty
        [Value::Bool(custom_do_trim), Value::Bool(custom_skip_empty)] => {
            do_trim = *custom_do_trim;
            skip_empty = *custom_skip_empty;
        }
        // (str, int): delimiter, maxsplit
        [Value::String(custom_delimiter), Value::Integer(custom_maxsplit)] => {
            delimiter = custom_delimiter.borrow_ref().unwrap().to_string();
            maxsplit = *custom_maxsplit;
        }
        // (str, int, bool): delimiter, maxsplit, do_trim
        [Value::String(custom_delimiter), Value::Integer(custom_maxsplit), Value::Bool(custom_do_trim)] =>
        {
            delimiter = custom_delimiter.borrow_ref().unwrap().to_string();
            maxsplit = *custom_maxsplit;
            do_trim = *custom_do_trim;
        }
        // (str, int, bool, bool): delimiter, maxsplit, do_trim, skip_empty
        [Value::String(custom_delimiter), Value::Integer(custom_maxsplit), Value::Bool(custom_do_trim), Value::Bool(custom_skip_empty)] =>
        {
            delimiter = custom_delimiter.borrow_ref().unwrap().to_string();
            maxsplit = *custom_maxsplit;
            do_trim = *custom_do_trim;
            skip_empty = *custom_skip_empty;
        }
        _ => panic!("Invalid arguments for read_split_lines_iter"),
    }
    SplitLinesIterator::new(path, &delimiter, maxsplit, do_trim, skip_empty)
}
