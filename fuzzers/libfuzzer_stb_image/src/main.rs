//! A libfuzzer-like fuzzer with llmp-multithreading support and restarts
//! The example harness is built for stb_image.

use std::{env, path::PathBuf};

use libafl::{
    bolts::tuples::tuple_list,
    corpus::{
        Corpus, InMemoryCorpus, IndexesLenTimeMinimizerCorpusScheduler, OnDiskCorpus,
        QueueCorpusScheduler,
    },
    events::setup_restarting_mgr_std,
    executors::{inprocess::InProcessExecutor, ExitKind},
    feedback_or,
    feedbacks::{CrashFeedback, MapFeedbackState, MaxMapFeedback, TimeFeedback},
    fuzzer::{Fuzzer, StdFuzzer},
    mutators::scheduled::{havoc_mutations, StdScheduledMutator},
    mutators::token_mutations::Tokens,
    observers::{StdMapObserver, TimeObserver},
    stages::mutational::StdMutationalStage,
    state::{HasCorpus, HasMetadata, StdState},
    stats::SimpleStats,
    utils::{current_nanos, StdRand},
    Error,
};

use libafl_targets::{libfuzzer_initialize, libfuzzer_test_one_input, EDGES_MAP, MAX_EDGES_NUM};

pub fn main() {
    // Registry the metadata types used in this fuzzer
    // Needed only on no_std
    //RegistryBuilder::register::<Tokens>();

    println!(
        "Workdir: {:?}",
        env::current_dir().unwrap().to_string_lossy().to_string()
    );
    fuzz(
        vec![PathBuf::from("./corpus")],
        PathBuf::from("./crashes"),
        1337,
    )
    .expect("An error occurred while fuzzing");
}

/// The actual fuzzer
fn fuzz(corpus_dirs: Vec<PathBuf>, objective_dir: PathBuf, broker_port: u16) -> Result<(), Error> {
    // 'While the stats are state, they are usually used in the broker - which is likely never restarted
    let stats = SimpleStats::new(|s| println!("{}", s));

    // The restarting state will spawn the same process again as child, then restarted it each time it crashes.
    let (state, mut restarting_mgr) = match setup_restarting_mgr_std(stats, broker_port) {
        Ok(res) => res,
        Err(err) => match err {
            Error::ShuttingDown => {
                return Ok(());
            }
            _ => {
                panic!("Failed to setup the restarter: {}", err);
            }
        },
    };

    // Create an observation channel using the coverage map
    // We don't use the hitcounts (see the Cargo.toml, we use pcguard_edges)
    let edges = unsafe { &mut EDGES_MAP[0..MAX_EDGES_NUM] };
    let edges_observer = StdMapObserver::new("edges", edges);

    // Create an observation channel to keep track of the execution time
    let time_observer = TimeObserver::new("time");

    // The state of the edges feedback.
    let feedback_state = MapFeedbackState::with_observer(&edges_observer);

    // Feedback to rate the interestingness of an input
    // This one is composed by two Feedbacks in OR
    let feedback = feedback_or!(
        // New maximization map feedback linked to the edges observer and the feedback state
        MaxMapFeedback::new_tracking(&feedback_state, &edges_observer, true, false),
        // Time feedback, this one does not need a feedback state
        TimeFeedback::new_with_observer(&time_observer)
    );

    // A feedback to choose if an input is a solution or not
    let objective = CrashFeedback::new();

    // If not restarting, create a State from scratch
    let mut state = state.unwrap_or_else(|| {
        StdState::new(
            // RNG
            StdRand::with_seed(current_nanos()),
            // Corpus that will be evolved, we keep it in memory for performance
            InMemoryCorpus::new(),
            // Corpus in which we store solutions (crashes in this example),
            // on disk so the user can get them after stopping the fuzzer
            OnDiskCorpus::new(objective_dir).unwrap(),
            // States of the feedbacks.
            // They are the data related to the feedbacks that you want to persist in the State.
            tuple_list!(feedback_state),
        )
    });

    println!("We're a client, let's fuzz :)");

    // Create a PNG dictionary if not existing
    if state.metadata().get::<Tokens>().is_none() {
        state.add_metadata(Tokens::new(vec![
            vec![137, 80, 78, 71, 13, 10, 26, 10], // PNG header
            "IHDR".as_bytes().to_vec(),
            "IDAT".as_bytes().to_vec(),
            "PLTE".as_bytes().to_vec(),
            "IEND".as_bytes().to_vec(),
        ]));
    }

    // Setup a basic mutator with a mutational stage
    let mutator = StdScheduledMutator::new(havoc_mutations());
    let mut stages = tuple_list!(StdMutationalStage::new(mutator));

    // A minimization+queue policy to get testcasess from the corpus
    let scheduler = IndexesLenTimeMinimizerCorpusScheduler::new(QueueCorpusScheduler::new());

    // A fuzzer with feedbacks and a corpus scheduler
    let mut fuzzer = StdFuzzer::new(scheduler, feedback, objective);

    // The wrapped harness function, calling out to the LLVM-style harness
    let mut harness = |buf: &[u8]| {
        libfuzzer_test_one_input(buf);
        ExitKind::Ok
    };

    // Create the executor for an in-process function with just one observer for edge coverage
    let mut executor = InProcessExecutor::new(
        &mut harness,
        tuple_list!(edges_observer, time_observer),
        &mut fuzzer,
        &mut state,
        &mut restarting_mgr,
    )?;

    // The actual target run starts here.
    // Call LLVMFUzzerInitialize() if present.
    let args: Vec<String> = env::args().collect();
    if libfuzzer_initialize(&args) == -1 {
        println!("Warning: LLVMFuzzerInitialize failed with -1")
    }

    // In case the corpus is empty (on first run), reset
    if state.corpus().count() < 1 {
        state
            .load_initial_inputs(
                &mut fuzzer,
                &mut executor,
                &mut restarting_mgr,
                &corpus_dirs,
            )
            .expect(&format!(
                "Failed to load initial corpus at {:?}",
                &corpus_dirs
            ));
        println!("We imported {} inputs from disk.", state.corpus().count());
    }

    fuzzer.fuzz_loop(&mut stages, &mut executor, &mut state, &mut restarting_mgr)?;

    // Never reached
    Ok(())
}