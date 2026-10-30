//! Executes independent resource lanes while preserving each lane's check order.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use super::PlannedCheck;

pub(super) fn resource_lanes(
    planned: Vec<PlannedCheck>,
    assignments: &[String],
    jobs: usize,
) -> io::Result<Vec<Vec<(usize, PlannedCheck)>>> {
    let mut resources = BTreeMap::new();
    for assignment in assignments {
        let (area, resource) = assignment.split_once('=').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "a lane must be AREA=RESOURCE")
        })?;
        if area.is_empty() || resource.is_empty() || resources.insert(area, resource).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lane areas must be nonempty and assigned once",
            ));
        }
    }
    let mut lanes: BTreeMap<String, Vec<(usize, PlannedCheck)>> = BTreeMap::new();
    for (index, check) in planned.into_iter().enumerate() {
        let resource = if jobs == 1 {
            "serial"
        } else {
            resources.get(check.area.as_str()).copied().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("parallel proof needs a lane for area {}", check.area),
                )
            })?
        };
        lanes
            .entry(resource.to_owned())
            .or_default()
            .push((index, check));
    }
    Ok(lanes.into_values().collect())
}

pub(super) fn execute<T: Send, F>(
    lanes: Vec<Vec<(usize, PlannedCheck)>>,
    jobs: usize,
    keep_going: bool,
    run: F,
) -> io::Result<Vec<(usize, T)>>
where
    F: Fn(usize, &PlannedCheck) -> io::Result<(bool, T)> + Sync,
{
    let workers = jobs.min(lanes.len());
    let waiting = Mutex::new(VecDeque::from(lanes));
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..workers {
            let waiting = &waiting;
            let stopped = &stopped;
            let run = &run;
            handles.push(scope.spawn(move || -> io::Result<Vec<(usize, T)>> {
                let mut completed = Vec::new();
                loop {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let lane = waiting
                        .lock()
                        .map_err(|_| io::Error::other("proof lane queue poisoned"))?
                        .pop_front();
                    let Some(lane) = lane else { break };
                    for (index, check) in lane {
                        if stopped.load(Ordering::Acquire) {
                            break;
                        }
                        let result = run(index, &check);
                        if result.is_err() {
                            stopped.store(true, Ordering::Release);
                        }
                        let (ok, value) = result?;
                        completed.push((index, value));
                        if !ok && !keep_going {
                            stopped.store(true, Ordering::Release);
                        }
                    }
                }
                Ok(completed)
            }));
        }
        let mut completed = Vec::new();
        let mut error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(results)) => completed.extend(results),
                Ok(Err(cause)) => error = Some(cause),
                Err(_) => error = Some(io::Error::other("proof worker panicked")),
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        completed.sort_by_key(|(index, _)| *index);
        Ok(completed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Duration;

    fn check(name: &str, area: &str) -> PlannedCheck {
        PlannedCheck {
            name: name.into(),
            cmd: "true".into(),
            area: area.into(),
            units: vec![],
        }
    }

    #[test]
    fn independent_lanes_run_together_and_each_shared_resource_stays_ordered() {
        let plan = vec![
            check("compile", "compile"),
            check("test", "test"),
            check("capture", "ui"),
        ];
        let lanes = resource_lanes(
            plan,
            &[
                "compile=target".into(),
                "test=target".into(),
                "ui=browser".into(),
            ],
            2,
        )
        .unwrap();
        let rendezvous = Arc::new(Barrier::new(2));
        let (send, receive) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = execute(lanes, 2, true, |index, _| {
                if index != 1 {
                    rendezvous.wait();
                }
                send.send(index).unwrap();
                Ok((true, index))
            });
            done.send(result).unwrap();
        });
        let result = finished
            .recv_timeout(Duration::from_secs(10))
            .expect("independent lanes did not rendezvous")
            .unwrap();
        worker.join().unwrap();
        assert_eq!(result, vec![(0, 0), (1, 1), (2, 2)]);
        let starts: Vec<_> = receive.try_iter().collect();
        assert!(starts.iter().position(|n| *n == 0) < starts.iter().position(|n| *n == 1));
    }

    #[test]
    fn parallel_checks_require_every_area_to_declare_its_resource() {
        assert!(resource_lanes(vec![check("compile", "rust")], &[], 2).is_err());
        assert!(resource_lanes(vec![check("compile", "rust")], &["rust=".into()], 2).is_err());
        assert!(resource_lanes(
            vec![check("compile", "rust")],
            &["rust=a".into(), "rust=b".into()],
            2
        )
        .is_err());
    }

    #[test]
    fn failure_stops_unscheduled_work_and_keep_going_preserves_all_results() {
        for keep in [false, true] {
            let lanes =
                resource_lanes(vec![check("first", "a"), check("second", "b")], &[], 1).unwrap();
            let results = execute(lanes, 1, keep, |index, _| Ok((index != 0, index))).unwrap();
            assert_eq!(results.len(), if keep { 2 } else { 1 });
        }
    }
}
