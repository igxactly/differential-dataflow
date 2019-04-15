extern crate rand;
extern crate timely;
extern crate differential_dataflow;
extern crate core_affinity;

use rand::{Rng, SeedableRng, StdRng};

use timely::dataflow::operators::{Exchange, Probe};
// use timely::progress::nested::product::Product;
// use timely::progress::timestamp::RootTimestamp;

use differential_dataflow::input::Input;
use differential_dataflow::operators::arrange::{Arrange, ArrangeBySelf, ArrangeByKey};
use differential_dataflow::operators::count::CountTotal;
use differential_dataflow::operators::threshold::ThresholdTotal;
use differential_dataflow::operators::{Join, JoinCore};

use differential_dataflow::trace::implementations::ord::OrdKeySpine;

#[derive(Debug)]
enum Comp {
    Nothing,
    Exchange,
    Arrange,
    Maintain,
    SelfJoin,
    Count,
    Distinct,
}

#[derive(Debug)]
enum Mode {
    OpenLoop,
    ClosedLoop,
}

#[derive(Debug)]
enum Duration {
    Overwrite(usize),
    Seconds(usize),
}

#[derive(Debug)]
enum ZeroCopy {
    No,
    Thread,
}

#[derive(Debug)]
enum Alloc {
    Jemalloc,
    JemallocAlloc,
}

#[derive(Debug)]
enum InputStrategy {
    Ms,
    PowerOfTwo,
}

fn main() {

    let mut args = std::env::args();
    args.next();

    let keys: usize = args.next().unwrap().parse().unwrap();
    let recs: usize = args.next().unwrap().parse().unwrap();
    let rate: usize = args.next().unwrap().parse().unwrap();
    let comp: Comp = match args.next().unwrap().as_str() {
        "exchange" => Comp::Exchange,
        "arrange" => Comp::Arrange,
        "maintain" => Comp::Maintain,
        "selfjoin" => Comp::SelfJoin,
        "count" => Comp::Count,
        "distinct" => Comp::Distinct,
        "nothing" => Comp::Nothing,
        _ => panic!("invalid comp"),
    };
    let mode: Mode = match args.next().unwrap().as_str() {
        "openloop" => Mode::OpenLoop,
        "closedloop" => Mode::ClosedLoop,
        _ => panic!("invalid mode"),
    };
    let duration: Duration = {
        let duration_mode = args.next().unwrap();
        let duration_param: usize = args.next().unwrap().parse().unwrap();
        match duration_mode.as_str() {
            "overwrite" => Duration::Overwrite(duration_param),
            "seconds" => Duration::Seconds(duration_param),
            _ => panic!("invalid duration mode"),
        }
    };
    let zerocopy: ZeroCopy = {
        let zerocopy_mode = args.next().unwrap();
        match zerocopy_mode.as_str() {
            "no" => ZeroCopy::No,
            "thread" => ZeroCopy::Thread,
            _ => panic!("boom"),
        }
    };
    let zerocopy_workers: usize = args.next().unwrap().parse().unwrap();

    let jemalloc: Alloc = {
        let jemalloc_mode = args.next().unwrap();
        match jemalloc_mode.as_str() {
            "jemalloc" => Alloc::Jemalloc,
            "jemallocalloc" => Alloc::JemallocAlloc,
            _ => panic!("boom"),
        }
    };

    let inputstrategy: InputStrategy = {
        let inputstrategy_mode = args.next().unwrap();
        match inputstrategy_mode.as_str() {
            "ms" => InputStrategy::Ms,
            "poweroftwo" => InputStrategy::PowerOfTwo,
            _ => panic!("boom"),
        }
    };

    // define a new computational scope, in which to run BFS
    macro_rules! worker_closure { () => (move |worker| {

        let tmp = match jemalloc {
            Alloc::Jemalloc => Vec::<usize>::new(),
            Alloc::JemallocAlloc => {
                eprintln!("jemalloc alloc!");
                Vec::<usize>::with_capacity(1 << 30)
            },
        };

        let index = worker.index();
        let core_ids = core_affinity::get_core_ids().unwrap();
        core_affinity::set_for_current(core_ids[index]);

        // create a a degree counting differential dataflow
        let (mut input, probe) = worker.dataflow::<u64,_,_>(|scope| {

            let (handle, data) = scope.new_collection();

            let probe = match comp {
                Comp::Nothing => data.probe(),
                Comp::Exchange => data.inner.exchange(|&(x,_,_): &((usize,()),_,_)| x.0 as u64).probe(),
                Comp::Arrange => data.arrange_by_key().stream.probe(),
                Comp::Maintain => data.arrange_by_key().join(&data.filter(|_| false)).probe(),
                Comp::SelfJoin => {
                    let arranged = data.arrange_by_key();
                    arranged.join_core(&arranged, |_key, &a, &b| if a == b { Some((a, b)) } else { None }).probe()
                },
                Comp::Count => data.arrange_by_key().count_total().probe(),
                Comp::Distinct => data.arrange_by_key().distinct_total().probe(),
            };

            // OrdKeySpine::<usize, Product<RootTimestamp,u64>,isize>::with_effort(work)

            (handle, probe)
        });

        let index = worker.index();
        let peers = worker.peers();

        let seed: &[_] = &[1, 2, 3, index];
        let mut rng1: StdRng = SeedableRng::from_seed(seed);    // rng for additions
        let mut rng2: StdRng = SeedableRng::from_seed(seed);    // rng for deletions

        let timer = ::std::time::Instant::now();

        for _ in 0 .. ((recs as usize) / peers) + if index < ((recs as usize) % peers) { 1 } else { 0 } {
            input.insert((rng1.gen_range(0, keys),()));
        }

        input.advance_to(1u64);
        input.flush();
        while probe.less_than(input.time()) { worker.step(); }

        if index == 0 {
            let elapsed1 = timer.elapsed();
            let elapsed1_ns = elapsed1.as_secs() * 1_000_000_000 + (elapsed1.subsec_nanos() as u64);
            // println!("{:?}\tdata loaded; rate: {:?}", elapsed1, 1000000000.0 * (recs as f64) / (elapsed1_ns as f64));
            println!("ARRANGE\tLOADING\t{}\t{:?}", peers, 1000000000.0 * (recs as f64) / (elapsed1_ns as f64));
        }

        if rate > 0 {

            let timer = ::std::time::Instant::now();
            // let mut counts = vec![0u64; 64];

            let mut counts = vec![[0u64; 16]; 64];

            match mode {

                // closed-loop latency-throughput test, parameterized by rate size.
                Mode::ClosedLoop => {

                    let mut wave = 1;
                    let mut elapsed = timer.elapsed();

                    let seconds = match duration {
                        Duration::Seconds(s) => s,
                        _ => panic!("invalid duration for closedloop"),
                    };
                    while elapsed.as_secs() < (seconds as u64) {

                        for round in 0 .. rate {
                            input.advance_to((((wave * rate) + round) * peers + index) as u64);
                            input.insert((rng1.gen_range(0, keys),()));
                            input.remove((rng2.gen_range(0, keys),()));
                        }
                        wave += 1;
                        input.advance_to((wave * rate * peers) as u64);
                        input.flush();

                        let elapsed1 = elapsed.clone();
                        let elapsed1_ns = elapsed1.as_secs() * 1_000_000_000 + (elapsed1.subsec_nanos() as u64);
                        while probe.less_than(input.time()) { worker.step(); }
                        elapsed = timer.elapsed();
                        let elapsed2 = elapsed.clone();
                        let elapsed2_ns = elapsed2.as_secs() * 1_000_000_000 + (elapsed2.subsec_nanos() as u64);
                        let count_index = (elapsed2_ns - elapsed1_ns).next_power_of_two().trailing_zeros() as usize;
                        if elapsed.as_secs() > 5 {
                            let low_bits = ((elapsed2_ns - elapsed1_ns) >> (count_index - 5)) & 0xF;
                            counts[count_index][low_bits as usize] += 1;
                        }
                    }

                    let elapsed = timer.elapsed();
                    let seconds = elapsed.as_secs() as f64 + (elapsed.subsec_nanos() as f64) / 1000000000.0;
                    if index == 0 {
                        // println!("{:?}, {:?}", seconds / (wave - 1) as f64, 2.0 * ((wave - 1) * rate * peers) as f64 / seconds);
                        println!("ARRANGE\tTHROUGHPUT\t{}\t{:?}\t{:?}", peers, 2.0 * ((wave - 1) * rate * peers) as f64 / seconds, mode);
                    }

                },
                Mode::OpenLoop => {

                    let requests_per_sec = rate / 2;
                    let ns_per_request = 1_000_000_000 / requests_per_sec;
                    let mut request_counter = peers + index;    // skip first request for each.
                    let mut ack_counter = peers + index;

                    let mut inserted_ns = 1;

                    let ack_target = match duration {
                        Duration::Overwrite(times) => times * keys,
                        Duration::Seconds(secs) => requests_per_sec * secs,
                    };
                    while ack_counter < ack_target {
                    // while ((timer.elapsed().as_secs() as usize) * rate) < (10 * keys) {

                        // Open-loop latency-throughput test, parameterized by offered rate `ns_per_request`.
                        let elapsed = timer.elapsed();
                        let elapsed_ns = elapsed.as_secs() * 1_000_000_000 + (elapsed.subsec_nanos() as u64);

                        // Determine completed ns.
                        let acknowledged_ns: u64 = probe.with_frontier(|frontier| frontier[0]);

                        // any un-recorded measurements that are complete should be recorded.
                        while ((ack_counter * ns_per_request) as u64) < acknowledged_ns && ack_counter < ack_target {
                            let requested_at = (ack_counter * ns_per_request) as u64;
                            let count_index = (elapsed_ns - requested_at).next_power_of_two().trailing_zeros() as usize;
                            if ack_counter > ack_target / 2 {
                                // counts[count_index] += 1;
                                let low_bits = ((elapsed_ns - requested_at) >> (count_index - 5)) & 0xF;
                                counts[count_index][low_bits as usize] += 1;
                            }
                            ack_counter += peers;
                        }

                        // Now, should we introduce more records before stepping the worker?
                        //
                        // Thinking: inserted_ns - acknowledged_ns is some amount of time that
                        // is currently outstanding in the system, and we needn't advance our
                        // inputs unless by this order of magnitude.
                        //
                        // The more sophisticated plan is: we compute the next power of two
                        // greater than inserted_ns - acknowledged_ns and look for the last
                        // multiple of this number in the interval [inserted_ns, elapsed_ns].
                        // If such a multiple exists, we introduce records to that point and
                        // advance the input.

                        // let scale = (inserted_ns - acknowledged_ns).next_power_of_two();
                        // max (scale / 4, 1024)
                        // let target_ns = elapsed_ns & !(scale - 1);

                        // let target_ns = if acknowledged_ns >= inserted_ns { elapsed_ns } else { inserted_ns };

                        // let target_ns = elapsed_ns & !((1 << 16) - 1);

                        let target_ns = match inputstrategy {
                            InputStrategy::Ms => {
                                let mut target_ns = elapsed_ns & !((1 << 20) - 1);
                                if target_ns > inserted_ns + 1_000_000_000 { target_ns = inserted_ns + 1_000_000_000; }
                                target_ns
                            },
                            InputStrategy::PowerOfTwo => {
                                let delta: u64 = inserted_ns - acknowledged_ns;
                                let bits = ::std::mem::size_of::<u64>() * 8 - delta.leading_zeros() as usize;
                                let scale = ::std::cmp::max((1 << bits) / 4, 1024);
                                elapsed_ns & !(scale - 1)
                            },
                        };

                        if inserted_ns < target_ns {

                            while ((request_counter * ns_per_request) as u64) < target_ns {
                                input.advance_to((request_counter * ns_per_request) as u64);
                                input.insert((rng1.gen_range(0, keys),()));
                                input.remove((rng2.gen_range(0, keys),()));
                                request_counter += peers;
                            }
                            input.advance_to(target_ns);
                            input.flush();
                            inserted_ns = target_ns;
                        }

                        worker.step();
                    }

                }
            }

            if index == 0 {

                let mut results = Vec::new();
                let total = counts.iter().map(|x| x.iter().sum::<u64>()).sum();
                let mut sum = 0;
                for index in (10 .. counts.len()).rev() {
                    for sub in (0 .. 16).rev() {
                        if sum > 0 && sum < total {
                            let latency = (1 << (index-1)) + (sub << (index-5));
                            let fraction = (sum as f64) / (total as f64);
                            results.push((latency, fraction));
                        }
                        sum += counts[index][sub];
                    }
                }
                for (latency, fraction) in results.drain(..).rev() {
                    println!("ARRANGE\tLATENCYALL\t{}\t{}\t{}\t{}\t{:?}\t{:?}\t{}\t{}", peers, keys, recs, rate, comp, mode, latency, fraction);
                    println!("ARRANGE\tLATENCYFRACTION\t{}\t{}", latency, fraction);
                }
            }
        }
    }) }

    match zerocopy {
        ZeroCopy::No => timely::execute_from_args(args, worker_closure!()).unwrap(),
        ZeroCopy::Thread => {
            eprintln!("thread allocators zerocopy");
            let allocators =
                ::timely::communication::allocator::zero_copy::allocator_process::ProcessBuilder::new_vector(zerocopy_workers);
            timely::execute::execute_from(allocators, Box::new(()), worker_closure!()).unwrap()
        },
    };

}
