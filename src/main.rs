// we use crossbeam instead of std::mpsc because it has a better API
// in particular we need multi-producer channels which we'd have to implement on
// top of mpsc ourselves without this
extern crate crossbeam_channel;
extern crate structopt;

use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::fs::ReadDir;
use std::io::{Result as IOResult, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};
use structopt::StructOpt;

#[derive(Clone, Copy, PartialEq)]
enum JobMessagePayload {
    JobStarted,
    RunStarted,
    RunFinished,
    JobFinished,
}

struct JobMessage(PathBuf, Instant, JobMessagePayload);

fn run_make(make_command: &str, working_directory: &Path) -> IOResult<ExitStatus> {
    use std::process::{Command, Stdio};
    Command::new("make")
        .arg(make_command)
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

fn run_proof(path: &Path, iterations: u32, sender: Sender<JobMessage>) {
    use JobMessagePayload::*;
    sender.send(JobMessage(path.to_path_buf(), Instant::now(), JobStarted));
    for _ in 0..iterations {
        run_make("veryclean", path).unwrap();
        run_make("goto", path).unwrap();
        sender
            .send(JobMessage(path.to_path_buf(), Instant::now(), RunStarted))
            .unwrap();
        run_make("result", path).unwrap();
        sender
            .send(JobMessage(path.to_path_buf(), Instant::now(), RunFinished))
            .unwrap();
    }
    sender.send(JobMessage(path.to_path_buf(), Instant::now(), JobFinished));
}

fn to_proof_dir(maybe_entry: IOResult<std::fs::DirEntry>) -> Option<PathBuf> {
    // A proof directory is any subdirectory of an AWS "proofs"
    // directory that contains a Makefile
    // We're just silently ignoring IO errors (like not having the right read permissions)
    // because these shouldn't come up in practice anyway.
    maybe_entry.ok().and_then(|entry| {
        if entry.path().join("Makefile").exists() {
            Some(entry.path())
        } else {
            None
        }
    })
}

fn start_proof_job(path: &Path, iterations: u32, sender: &Sender<JobMessage>) {
    use std::thread::spawn;
    let job_sender = sender.clone();
    let job_path = path.to_path_buf();
    spawn(move || run_proof(&job_path, iterations, job_sender));
}

// run all proofs in proofs_path in parallel with parallel_jobs parallel jobs and send run messages
// to sender.
fn run_all_proofs_in(
    proofs_path: &Path,
    iterations: u32,
    parallel_jobs: u32,
    sender: Sender<JobMessage>,
) -> IOResult<usize> {
    use std::fs::read_dir;
    use std::thread::spawn;
    let proof_dirs = {
        let mut proof_dirs_mut: Vec<PathBuf> =
            read_dir(proofs_path)?.filter_map(to_proof_dir).collect();
        proof_dirs_mut.sort();
        proof_dirs_mut
    };
    let nr_of_proofs = proof_dirs.len();
    spawn(move || {
        let (relay_sender, relay_receiver) = crossbeam_channel::unbounded();

        let initial_proof_jobs = proof_dirs.len().min(parallel_jobs as usize);

        // spawn the first <parallel-jobs> jobs
        for i in 0..initial_proof_jobs {
            start_proof_job(&proof_dirs[i], iterations, &relay_sender);
        }

        let mut finished_jobs = 0;
        // wait for a job to finish before starting the next one
        for proof_dir in proof_dirs.iter().skip(initial_proof_jobs) {
            use JobMessagePayload::JobFinished;
            loop {
                let job_message = relay_receiver.recv().unwrap();
                let is_job_finished = job_message.2 == JobFinished;
                sender.send(job_message);
                if is_job_finished {
                    finished_jobs += 1;
                    break;
                }
            }
            start_proof_job(&proof_dir, iterations, &relay_sender);
        }

        // with the way the above loop is structured we'll normally have jobs (at least 1)
        // remaining at the end who's messages we also need to relay
        for _ in 0..(nr_of_proofs - finished_jobs) {
            use JobMessagePayload::JobFinished;
            loop {
                let job_message = relay_receiver.recv().unwrap();
                let is_job_finished = job_message.2 == JobFinished;
                sender.send(job_message);
                if is_job_finished {
                    break;
                }
            }
        }
    });
    Ok(nr_of_proofs)
}

fn dump_csv<'a, RunResults: Iterator<Item = (&'a PathBuf, &'a Vec<Duration>)>>(
    run_results: RunResults,
    csv_path: &Path,
) {
    let mut csv_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(csv_path)
        .expect("csv_file should exist");
    for (path, runs) in run_results {
        csv_file
            .write(
                format!(
                    "{}",
                    path.to_str()
                        .expect("paths should be convertible to unicode")
                )
                .as_bytes(),
            )
            .unwrap();
        for run in runs {
            csv_file
                .write(format!(",{}", run.as_secs_f32()).as_bytes())
                .unwrap();
        }
        csv_file.write("\n".as_bytes()).unwrap();
    }
}

fn benchmark_all_proofs_in(
    path: &Path,
    iterations: u32,
    parallel_jobs: u32,
    csv_path: &Path,
) -> IOResult<()> {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let mut proof_runtimes: HashMap<PathBuf, Vec<Duration>> = HashMap::new();
    let mut started_runs: HashMap<PathBuf, Instant> = HashMap::new();
    let nr_of_jobs = run_all_proofs_in(path, iterations, parallel_jobs, sender.clone())?;
    let mut completed_jobs = 0;
    while completed_jobs < nr_of_jobs {
        use JobMessagePayload::*;
        let JobMessage(proof_path, timestamp, message_type) = receiver.recv().unwrap();
        match message_type {
            JobStarted => {
                proof_runtimes.insert(proof_path, Vec::new());
            }
            JobFinished => {
                completed_jobs += 1;
                println!("COMPLETED [{}/{}] jobs", completed_jobs, nr_of_jobs);
            }
            RunStarted => {
                started_runs.insert(proof_path, timestamp);
            }
            RunFinished => {
                let start_time = started_runs
                    .remove(&proof_path)
                    .expect("we cannot finish a run we didn't start first");
                let runtime = timestamp - start_time;
                println!(
                    "{} finished after {}s",
                    proof_path.to_str().unwrap(),
                    runtime.as_secs_f32()
                );
                proof_runtimes
                    .get_mut(&proof_path)
                    .expect("we cannot finish a run in a job that hasn't started yet")
                    .push(runtime);
            }
        }
    }
    dump_csv(proof_runtimes.iter(), csv_path);
    Ok(())
}

#[derive(StructOpt)]
struct Arguments {
    #[structopt(long, parse(from_os_str))]
    proofs_path: PathBuf,
    #[structopt(long)]
    iterations: u32,
    #[structopt(long)]
    parallel_jobs: u32,
    #[structopt(long, parse(from_os_str))]
    csv_file: PathBuf,
}

fn main() -> IOResult<()> {
    let args = Arguments::from_args();

    benchmark_all_proofs_in(
        &args.proofs_path,
        args.iterations,
        args.parallel_jobs,
        &args.csv_file,
    )
}
