extern crate logparse;
extern crate timely;
extern crate timely_query;
extern crate time;

#[macro_use] extern crate sessionize;
extern crate sessionize_shared;

use logparse::parser2::{GetDcxidTrxnb, GetMsgTypeIP};
use logparse::reorder::RecordReorder;

use sessionize::sessionize::Sessionize;

use timely::dataflow::Scope;
use timely::dataflow::operators::*;
use timely::progress::timestamp::RootTimestamp;

use timely_query::publish::Partition as Topics;

use sessionize_shared::Message;
use sessionize_shared::reader::{locate_log_runs, get_max_fd_limit, open_file_readers_for_worker};
use sessionize_shared::monitor::ThroughputPerSec;

/// Window for the reorder buffer
const MAX_ALLOWED_OUT_OF_ORDERNESS: u64 = 10;  // unit: seconds

/// Interval spanned by each dataflow epoch, specified in terms of log timestamps
const EPOCH_DURATION: u64 = 1_000_000;  // unit: microseconds

/// Duration after which an inactive session is terminated
const SESSION_INACTIVITY_LIMIT: u64 = 5_000_000;  // unit: microseconds

fn main() {
    let start = time::precise_time_ns();

    let prefix = ::std::env::args().nth(1)
        .expect("need to pass the prefix to the logs as the first argument");

    let logdir = ::std::path::PathBuf::from(::std::env::args().nth(2)
        .expect("second arg needs to be logdir"));

    println!("starting analysis with prefix: {}", prefix);
    let inputs = locate_log_runs(prefix, true); // `true` for follow_symlinks
    if let Some(limit) = get_max_fd_limit() {
        if (inputs.len() as u64) > limit {
            println!("WARNING: file descriptor limit is too low ({} inputs but max is {}); \
                    run `ulimit -n 2048` to increase it", inputs.len(), limit);
        }
    }

    timely_query::execute(move |computation, coord| {
        let peers = computation.peers();
        let worker_index = computation.index();
        let (mut input, probe) = computation.scoped::<u64,_,_>(|scope| {
            let (input, stream) = scope.new_input();
            // Root Query: Messages per ssession
            let sessionize = stream.sessionize(EPOCH_DURATION, SESSION_INACTIVITY_LIMIT);
            sessionize.throughput_per_sec(logdir.join(format!("sessionize_out.{}.csv", worker_index)));
            coord.publish("sessionize", &sessionize, Topics::PerWorker).unwrap();

            let probe = sessionize.probe().0;
            (input, probe)
        });

        let worker_inputs = open_file_readers_for_worker(&inputs, worker_index, peers);
        let file_count = worker_inputs.iter().map(|chain| chain.path_count).fold(0, |acc, c| acc + c);
        println!("Worker {}: {} log runs across {} files", worker_index, worker_inputs.len(), file_count);

        let mut ordered = worker_inputs.into_iter()
            .map(|x| RecordReorder::new(x, MAX_ALLOWED_OUT_OF_ORDERNESS, |rec| rec.timestamp.to_epoch_seconds() as u64).peekable())
            .collect::<Vec<_>>();

        let mut index = 0;
        while index < ordered.len() {
            if ordered[index].peek().is_none() {
                drop(ordered.remove(index));
            }
            else {
                index += 1;
            }
        }

        while ordered.len() > 0 {
            let input_start = time::precise_time_ns();

            // determine next smallest time to play
            let min_time = ordered.iter_mut().map(|x| x.peek().unwrap().timestamp.to_epoch_seconds()).min().unwrap() as u64;

            // advance input time
            if input.epoch() < &min_time {    // NOTE: Asserts otherwise
                input.advance_to(min_time);
            }

            assert!(input.epoch() == &min_time);

            // drain records with this time from each iterator
            for iterator in &mut ordered {
                while iterator.peek().is_some() && iterator.peek().unwrap().timestamp.to_epoch_seconds() as u64 == min_time {
                    let record = iterator.next().unwrap();
                    if let Some((dcx, trxnb)) = record.get_dcxid_trxnb() {
                        if let Some((msg_tag, ip, _, _)) = record.get_msg_type_ip_cor() {
                            input.send(Message::new(dcx.to_owned(), trxnb.to_owned(), 
                                        msg_tag.to_string(), ip.to_string(), 
                                        record.timestamp.micros as u64));
                        }
                    }
                }
            }

            // discard iterators with no more records
            let mut index = 0;
            while index < ordered.len() {
                if ordered[index].peek().is_none() {
                    drop(ordered.remove(index));
                }
                else {
                    index += 1;
                }
            }

            // advance input time
            if min_time > 0 {
                input.advance_to(min_time + 1);
            }

            let process_start = time::precise_time_ns();
            while probe.le(&RootTimestamp::new(min_time)) {
                computation.step();
            }

            let iter_end = time::precise_time_ns();
            println!("iter:sessionize,{},{},{},{}", worker_index, input_start, process_start, iter_end);
        }
    }).unwrap();

    let end = time::precise_time_ns();
    println!("total:sessionize,{},{}", start, end);
}
