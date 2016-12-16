extern crate timely_query;
extern crate env_logger;
extern crate getopts;

use std::fs;
use std::io::{self, Write};
use std::env;
use std::process;

use getopts::{Options, Matches, ParsingStyle};

use timely_query::coordinator::requests::*;
use timely_query::model::*;
use timely_query::network::Network;
use timely_query::submit::Submitter;

fn usage(opts: Options, err: Option<String>) -> ! {
    let program = env::args().next().unwrap_or(String::from("executor"));
    let brief = format!("Usage: {} [options] -p FILE [<args>...]", program);

    if let Some(msg) = err {
        writeln!(io::stderr(), "{}\n\n{}", msg, brief).unwrap();
        process::exit(1);
    } else {
        println!("{}", opts.usage(&brief));
        process::exit(0);
    }
}

fn parse_placement(m: Matches, executors: Vec<Executor>) -> Result<Placement, String> {
    fn fmt_err<E: ToString>(opt: &str, err: E) -> String {
        format!("failed to parse argument for '{}': {}", opt, err.to_string())
    }

    let workers = if let Some(w) = m.opt_str("w") {
        w.parse().map_err(|e| fmt_err("workers", e))?
    } else {
        1
    };

    let placement = match (m.opt_str("r"), m.opt_str("f"), m.opt_str("n")) {
        (Some(r), None, None) => {
            let executors = r.parse().map_err(|e| fmt_err("random", e))?;
            Placement::Random(executors, workers)
        },
        (None, Some(f), None) => {
            let mut fixed = vec![];
            for num in f.split(",") {
                let id = num.parse::<u64>().map_err(|e| fmt_err("fixed", e))?;
                fixed.push(ExecutorId(id));
            }
            Placement::Fixed(fixed, workers)
        }
        (None, None, Some(n)) => {
            let mut fixed = vec![];
            for name in n.split(",") {
                let mut executor = executors.iter().filter(|e| e.host == name);
                if let Some(executor) = executor.next() {
                    fixed.push(executor.id);
                } else {
                    return Err(format!("unknown executor '{}'", name))
                }
            }
            Placement::Fixed(fixed, workers)
        }
        (None, None, None) => {
            Placement::Random(1, workers)
        }
        _ => {
            let err = "Options 'random', 'fixed', and 'hosts' are mutually exclusive";
            return Err(String::from(err));
        }
    };
    
    Ok(placement)
}

fn main() {
    drop(env_logger::init());
    
    let args: Vec<String> = env::args().skip(1).collect();
    let mut options = Options::new();
    options
        .parsing_style(ParsingStyle::StopAtFirstFree)
        .optflag("h", "help", "display this help and exit")
        .optopt("p", "program", "path to the query binary", "FILE")
        .optopt("c", "coordinator", "address of the coordinator", "ADDR")
        .optopt("w", "workers", "number of per-process worker threads", "NUM")
        .optopt("r", "random", "number of executors selected randomly", "NUM")
        .optopt("f", "fixed", "comma-separated list of executor ids", "LIST")
        .optopt("n", "hosts", "comma-separated executor hostnames", "LIST")
        .optopt("d", "desc", "optional query description", "DESC")
        .optflag("l", "local", "use local file path for submission");

    let m = match options.parse(&args) {
        Ok(m) => m,
        Err(f) => usage(options, Some(f.to_string()))
    };

    if m.opt_present("h") {
        usage(options, None);
    }
    
    if !m.opt_present("p") {
        usage(options, Some(format!("missing 'program' argument'")));
    }

    let binary = fs::canonicalize(m.opt_str("p").expect("missing binary"))
        .and_then(|path| {
            path.to_str()
                .map(ToString::to_string)
                .ok_or(io::Error::new(io::ErrorKind::Other, "utf-8 encoding error"))
        });

    let source = match binary {
        Ok(path) => path,
        Err(err) => usage(options, Some(err.to_string())),
    };

    let local = m.opt_present("l");

    let coord = m.opt_str("c").unwrap_or(String::from("localhost:9189"));
    let desc = m.opt_str("d");
    let args = m.free.clone();

    let network = Network::init().unwrap();
    let submitter = Submitter::new(&network, &*coord)
        .expect("cannot connect to coordinator");
    let executors = submitter.executors()
        .expect("coordinator topic unreachable (incorrect external hostname?)");

    // assemble the placement configuration
    let placement = match parse_placement(m, executors) {
        Ok(placement) => placement,
        Err(err) => usage(options, Some(err)),
    };

    // prepare location for executors to fetch binary
    let (url, upload) = if local {
        (format!("file://{}", source), None)
    } else {
        let handle = network.upload(source).unwrap();
        (handle.url(), Some(handle))
    };

    let query = QueryProgram {
        source: url,
        format: ExecutionFormat::NativeExecutable,
        args: args,
    };

    let id = submitter.submit(query, desc, placement).wait_unwrap();
    println!("spawned query: {:?}", id);
    
    drop(upload)
}
