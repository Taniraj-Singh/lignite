//! `lignite` — a small command-line shell over the storage engine.
//!
//! The binary opens one in-memory database and drives it from a line-oriented
//! command language, read either from a script file named on the command line or
//! interactively from standard input. It is a thin, dependency-free front end
//! meant for demos, manual poking and scripted smoke tests; every command maps
//! directly onto a public engine operation.
//!
//! ```text
//! lignite                 # interactive shell reading commands from stdin
//! lignite script.txt      # run the commands in script.txt, then exit
//! lignite --help          # print usage
//! ```
//!
//! ## Command language
//!
//! One command per line; blank lines and lines beginning with `#` are ignored.
//! Arguments are whitespace-separated, with `"..."` quoting for values that
//! contain spaces and the usual `\n`, `\t`, `\\` and `\"` escapes inside quotes.
//!
//! | Command | Effect |
//! |---|---|
//! | `put KEY VALUE` | store `VALUE` under `KEY` |
//! | `get KEY` | print the value or `(nil)` |
//! | `del KEY` | delete `KEY` |
//! | `scan [PREFIX]` | print every key/value, optionally filtered by prefix |
//! | `count [PREFIX]` | print how many keys match |
//! | `flush` / `compact` | force a memtable flush / full compaction |
//! | `stats` | print engine counters |
//! | `help` | list commands |
//! | `quit` / `exit` | leave the shell |

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use lignite::{Db, LigniteIterator, Options, ReadOptions, WriteOptions};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return ExitCode::SUCCESS;
    }
    if args.len() > 1 {
        eprintln!("lignite: expected at most one script path; see --help");
        return ExitCode::FAILURE;
    }

    let mut shell = match Shell::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lignite: failed to open database: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = match args.first() {
        Some(path) => shell.run_script(path),
        None => shell.run_interactive(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lignite: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!("lignite — command-line shell for the lignite storage engine\n");
    println!("usage:");
    println!("  lignite            run an interactive shell (commands on stdin)");
    println!("  lignite SCRIPT     run the commands in SCRIPT, then exit");
    println!("  lignite --help     print this message\n");
    print_commands();
}

fn print_commands() {
    println!("commands:");
    println!("  put KEY VALUE      store VALUE under KEY");
    println!("  get KEY            print the value or (nil)");
    println!("  del KEY            delete KEY");
    println!("  scan [PREFIX]      list key/value pairs, optionally by prefix");
    println!("  count [PREFIX]     count keys, optionally by prefix");
    println!("  flush              force the memtable to an SSTable");
    println!("  compact            merge all tables into one run");
    println!("  stats              print engine counters");
    println!("  help               show this list");
    println!("  quit | exit        leave the shell");
}

/// The interactive session: one database plus the reusable option bundles.
struct Shell {
    db: Db,
    wo: WriteOptions,
    ro: ReadOptions,
}

impl Shell {
    fn open() -> lignite::Result<Shell> {
        Ok(Shell {
            db: Db::open("lignite-cli", Options::default())?,
            wo: WriteOptions::default(),
            ro: ReadOptions::default(),
        })
    }

    /// Execute every line of a script file, stopping on the first hard error.
    fn run_script(&mut self, path: &str) -> lignite::Result<()> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| lignite::Error::io(format!("reading script {path}"), &e))?;
        for (n, line) in text.lines().enumerate() {
            match self.dispatch(line) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Quit) => break,
                Err(msg) => {
                    return Err(lignite::Error::invalid(format!(
                        "{path}:{}: {msg}",
                        n + 1
                    )));
                }
            }
        }
        Ok(())
    }

    /// Read-eval-print loop over standard input.
    fn run_interactive(&mut self) -> lignite::Result<()> {
        let stdin = io::stdin();
        let mut out = io::stdout();
        let interactive = true;
        loop {
            if interactive {
                let _ = write!(out, "lignite> ");
                let _ = out.flush();
            }
            let mut line = String::new();
            let read = stdin
                .lock()
                .read_line(&mut line)
                .map_err(|e| lignite::Error::io("reading stdin", &e))?;
            if read == 0 {
                break; // EOF
            }
            match self.dispatch(&line) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Quit) => break,
                Err(msg) => println!("error: {msg}"),
            }
        }
        Ok(())
    }

    /// Parse and run one command line. Returns whether to keep going, or a
    /// human-readable error string for a malformed or failed command.
    fn dispatch(&mut self, line: &str) -> std::result::Result<Flow, String> {
        let tokens = tokenize(line)?;
        let (cmd, rest) = match tokens.split_first() {
            Some(x) => x,
            None => return Ok(Flow::Continue), // blank or comment
        };

        match cmd.as_str() {
            "put" => {
                let (key, value) = two_args(rest, "put KEY VALUE")?;
                self.db
                    .put(&self.wo, key.as_bytes(), value.as_bytes())
                    .map_err(|e| e.to_string())?;
                println!("OK");
            }
            "get" => {
                let key = one_arg(rest, "get KEY")?;
                match self.db.get(&self.ro, key.as_bytes()).map_err(|e| e.to_string())? {
                    Some(v) => println!("{}", String::from_utf8_lossy(&v)),
                    None => println!("(nil)"),
                }
            }
            "del" | "delete" => {
                let key = one_arg(rest, "del KEY")?;
                self.db
                    .delete(&self.wo, key.as_bytes())
                    .map_err(|e| e.to_string())?;
                println!("OK");
            }
            "scan" => {
                let prefix = rest.first().map(|s| s.as_bytes().to_vec());
                let n = self.for_each(prefix.as_deref(), |k, v| {
                    println!(
                        "{}\t{}",
                        String::from_utf8_lossy(k),
                        String::from_utf8_lossy(v)
                    );
                })
                .map_err(|e| e.to_string())?;
                println!("({n} rows)");
            }
            "count" => {
                let prefix = rest.first().map(|s| s.as_bytes().to_vec());
                let n = self
                    .for_each(prefix.as_deref(), |_, _| {})
                    .map_err(|e| e.to_string())?;
                println!("{n}");
            }
            "flush" => {
                self.db.flush().map_err(|e| e.to_string())?;
                println!("OK");
            }
            "compact" => {
                let stats = self.db.compact().map_err(|e| e.to_string())?;
                println!(
                    "compacted {} files -> {} files",
                    stats.input_files, stats.output_files
                );
            }
            "stats" => {
                let s = self.db.stats();
                println!("{s:#?}");
                println!("live_files\t{}", self.db.live_file_count());
                println!("sequence\t{}", self.db.latest_sequence());
            }
            "help" => print_commands(),
            "quit" | "exit" => return Ok(Flow::Quit),
            other => {
                return Err(format!("unknown command '{other}' (try 'help')"));
            }
        }
        Ok(Flow::Continue)
    }

    /// Apply `f` to every visible key/value pair, optionally filtered by an exact
    /// byte prefix. Returns the number of pairs visited.
    fn for_each(
        &self,
        prefix: Option<&[u8]>,
        mut f: impl FnMut(&[u8], &[u8]),
    ) -> lignite::Result<usize> {
        let mut it = self.db.new_iterator(&self.ro)?;
        it.seek_to_first();
        let mut n = 0;
        while it.valid() {
            let key = it.key();
            let matches = prefix.map(|p| key.starts_with(p)).unwrap_or(true);
            if matches {
                f(key, it.value());
                n += 1;
            }
            it.next();
        }
        it.status()?;
        Ok(n)
    }
}

/// What to do after a command.
enum Flow {
    Continue,
    Quit,
}

/// Require exactly one argument.
fn one_arg<'a>(rest: &'a [String], usage: &str) -> std::result::Result<&'a String, String> {
    match rest {
        [a] => Ok(a),
        _ => Err(format!("usage: {usage}")),
    }
}

/// Require exactly two arguments.
fn two_args<'a>(
    rest: &'a [String],
    usage: &str,
) -> std::result::Result<(&'a String, &'a String), String> {
    match rest {
        [a, b] => Ok((a, b)),
        _ => Err(format!("usage: {usage}")),
    }
}

/// Split a command line into tokens, honouring `#` comments, double-quoted
/// strings and `\n \t \\ \"` escapes inside quotes.
fn tokenize(line: &str) -> std::result::Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '#' if !in_token => break, // comment to end of line
            c if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            '"' => {
                in_token = true;
                // Consume the quoted body.
                loop {
                    match chars.next() {
                        None => return Err("unterminated quoted string".to_string()),
                        Some('"') => break,
                        Some('\\') => {
                            let e = chars
                                .next()
                                .ok_or_else(|| "dangling escape in string".to_string())?;
                            cur.push(match e {
                                'n' => '\n',
                                't' => '\t',
                                '\\' => '\\',
                                '"' => '"',
                                other => other,
                            });
                        }
                        Some(ch) => cur.push(ch),
                    }
                }
            }
            other => {
                in_token = true;
                cur.push(other);
            }
        }
    }
    if in_token {
        tokens.push(cur);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_plain_words() {
        assert_eq!(tokenize("put a b").unwrap(), vec!["put", "a", "b"]);
    }

    #[test]
    fn tokenize_ignores_comment() {
        assert!(tokenize("   # a comment").unwrap().is_empty());
        assert_eq!(tokenize("get x # trailing").unwrap(), vec!["get", "x"]);
    }

    #[test]
    fn tokenize_quoted_with_spaces() {
        assert_eq!(
            tokenize(r#"put k "hello world""#).unwrap(),
            vec!["put", "k", "hello world"]
        );
    }

    #[test]
    fn tokenize_escapes() {
        assert_eq!(
            tokenize(r#"put k "a\tb\nc""#).unwrap(),
            vec!["put", "k", "a\tb\nc"]
        );
    }

    #[test]
    fn tokenize_unterminated_is_error() {
        assert!(tokenize(r#"put k "oops"#).is_err());
    }

    #[test]
    fn shell_put_get_del_flow() {
        let mut sh = Shell::open().unwrap();
        assert!(matches!(sh.dispatch("put greeting hello"), Ok(Flow::Continue)));
        assert!(matches!(sh.dispatch("get greeting"), Ok(Flow::Continue)));
        assert!(matches!(sh.dispatch("del greeting"), Ok(Flow::Continue)));
        assert!(matches!(sh.dispatch("count"), Ok(Flow::Continue)));
    }

    #[test]
    fn shell_reports_unknown_command() {
        let mut sh = Shell::open().unwrap();
        assert!(sh.dispatch("frobnicate x").is_err());
    }

    #[test]
    fn shell_scan_prefix_counts_correctly() {
        let mut sh = Shell::open().unwrap();
        sh.dispatch("put user:1 a").unwrap();
        sh.dispatch("put user:2 b").unwrap();
        sh.dispatch("put admin:1 c").unwrap();
        // Two keys share the "user:" prefix.
        let n = sh.for_each(Some(b"user:"), |_, _| {}).unwrap();
        assert_eq!(n, 2);
        let all = sh.for_each(None, |_, _| {}).unwrap();
        assert_eq!(all, 3);
    }

    #[test]
    fn quit_stops_the_loop() {
        let mut sh = Shell::open().unwrap();
        assert!(matches!(sh.dispatch("quit"), Ok(Flow::Quit)));
    }
}
