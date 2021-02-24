// we use crossbeam instead of std::mpsc because it has a better API
// in particular we need multi-producer channels which we'd have to implement on
// top of mpsc ourselves without this
extern crate crossbeam_channel;
extern crate structopt;

use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{Result as IOResult, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};
use structopt::StructOpt;

type GenericResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, PartialEq)]
enum JobMessagePayload {
    JobStarted,
    RunStarted,
    RunFinished,
    RunFailed,
    JobFinished,
}

struct JobMessage(PathBuf, Instant, JobMessagePayload);

struct RunProofMessage {
    job_path: PathBuf,
    iterations: u32,
}

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

fn run_proof(path: &Path, iterations: u32, sender: &Sender<JobMessage>) {
    use JobMessagePayload::*;
    sender
        .send(JobMessage(path.to_path_buf(), Instant::now(), JobStarted))
        .expect("Receiver shouldn't die while we're still sending messages");
    for _ in 0..iterations {
        sender
            .send(JobMessage(path.to_path_buf(), Instant::now(), RunStarted))
            .expect("Receiver shouldn't die while we're still sending messages");
        if let Ok(_status) = run_make("result", path) {
            sender
                .send(JobMessage(path.to_path_buf(), Instant::now(), RunFinished))
                .expect("Receiver shouldn't die while we're still sending messages");
        } else {
            sender
                .send(JobMessage(path.to_path_buf(), Instant::now(), RunFailed))
                .expect("Receiver shouldn't die while we're still sending messages");
        }
    }
    sender
        .send(JobMessage(path.to_path_buf(), Instant::now(), JobFinished))
        .expect("Receiver shouldn't die while we're still sending messages");
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

fn start_proof_job(receiver: &Receiver<RunProofMessage>, sender: &Sender<JobMessage>) {
    use std::thread::spawn;
    let job_sender = sender.clone();
    let job_receiver = receiver.clone();
    spawn(move || {
        while let Ok(run_proof_message) = job_receiver.recv() {
            run_proof(
                &run_proof_message.job_path,
                run_proof_message.iterations,
                &job_sender,
            );
        }
    });
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
    let nr_of_jobs = proof_dirs.len();
    let (job_run_sender, job_run_receiver) = crossbeam_channel::unbounded();
    // spawn the first <parallel-jobs> jobs
    for _ in 0..parallel_jobs {
        start_proof_job(&job_run_receiver, &sender);
    }

    // wait for a job to finish before starting the next one
    for proof_dir in proof_dirs.iter() {
        job_run_sender
            .send(RunProofMessage {
                job_path: proof_dir.clone(),
                iterations,
            })
            .expect("there should be always at least one job listening to job run requests");
    }
    Ok(nr_of_jobs)
}

fn dump_csv<'a, RunResults: Iterator<Item = &'a Option<Duration>>>(
    job_name: &str,
    run_results: RunResults,
    csv_file: &mut File,
) -> IOResult<()> {
    csv_file.write(job_name.as_bytes())?;
    for run in run_results {
        csv_file.write(",".as_bytes())?;
        if let Some(runtime) = run {
            csv_file.write(format!("{}", runtime.as_secs_f32()).as_bytes())?;
        }
    }
    csv_file.write("\n".as_bytes())?;
    csv_file.flush()
}

fn benchmark_all_proofs_in(
    path: &Path,
    iterations: u32,
    parallel_jobs: u32,
    csv_path: &Path,
) -> GenericResult<()> {
    let mut csv_file = OpenOptions::new().create(true).write(true).open(csv_path)?;
    let (sender, receiver) = crossbeam_channel::unbounded();
    let mut proof_runtimes: HashMap<PathBuf, Vec<Option<Duration>>> = HashMap::new();
    let mut started_runs: HashMap<PathBuf, Instant> = HashMap::new();
    let nr_of_jobs = run_all_proofs_in(path, iterations, parallel_jobs, sender)?;
    let mut completed_jobs = 0;
    while let Ok(JobMessage(proof_path, timestamp, message_type)) = receiver.recv() {
        let job_name = proof_path
            .file_name()
            .expect("proof paths do not end in ..")
            .to_str()
            .expect("paths should be convertible to utf-8");
        use JobMessagePayload::*;
        match message_type {
            JobStarted => {
                println!("STARTING {}", job_name);
                proof_runtimes.insert(proof_path, Vec::new());
            }
            JobFinished => {
                completed_jobs += 1;
                dump_csv(job_name, proof_runtimes[&proof_path].iter(), &mut csv_file)?;
                println!("COMPLETED [{}/{}] jobs", completed_jobs, nr_of_jobs);
            }
            RunStarted => {
                started_runs.insert(proof_path.clone(), timestamp);
                let run_nr = proof_runtimes
                    .get(&proof_path)
                    .expect("can not start a run for a job that hasn't started yet")
                    .len()
                    + 1;
                println!("STARTING RUN [{}/{}] for {}", run_nr, iterations, job_name);
            }
            RunFailed => {
                let start_time = started_runs
                    .remove(&proof_path)
                    .expect("we cannot finish a run we didn't start first");
                let runtime = Instant::now() - start_time;
                let proof_runtime = proof_runtimes
                    .get_mut(&proof_path)
                    .expect("we cannot fail a run for a job that hasn't been started");
                println!(
                    "FAILED RUN [{}/{}] for {} after {}s",
                    proof_runtime.len(),
                    iterations,
                    job_name,
                    runtime.as_secs_f32()
                );
                proof_runtime.push(None);
            }
            RunFinished => {
                let start_time = started_runs
                    .remove(&proof_path)
                    .expect("we cannot finish a run we didn't start first");
                let runtime = timestamp - start_time;
                let proof_runtime = proof_runtimes
                    .get_mut(&proof_path)
                    .expect("we cannot finish a run in a job that hasn't started yet");
                proof_runtime.push(Some(runtime));
                println!(
                    "FINISHED RUN [{}/{}] for {} after {}s",
                    proof_runtime.len(),
                    iterations,
                    job_name,
                    runtime.as_secs_f32()
                );
            }
        }
    }
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

fn main() -> GenericResult<()> {
    let args = Arguments::from_args();

    benchmark_all_proofs_in(
        &args.proofs_path,
        args.iterations,
        args.parallel_jobs,
        &args.csv_file,
    )
}
