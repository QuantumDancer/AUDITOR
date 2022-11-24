// Copyright 2021-2022 AUDITOR developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::{collections::HashMap, fmt};

use auditor::{
    constants::FORBIDDEN_CHARACTERS,
    domain::{Component, RecordAdd, Score},
};
use chrono::{DateTime, FixedOffset, Local, Utc};
use color_eyre::eyre::{eyre, Result};
use itertools::Itertools;
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::{process::Command, sync::mpsc};

use crate::{
    configuration::{AllowedTypes, Settings},
    database::Database,
    shutdown::Shutdown,
    CONFIG, KEYS,
};

type Job = HashMap<String, AllowedTypes>;

static BATCH_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^[0-9]+\.batch$"#).expect("Could not construct essential Regex"));

#[tracing::instrument(
    name = "Starting sacct monitor",
    skip(database, tx, _shutdown_notifier, shutdown, hold_till_shutdown)
)]
pub(crate) async fn run_sacct_monitor(
    database: Database,
    tx: mpsc::Sender<RecordAdd>,
    _shutdown_notifier: mpsc::UnboundedSender<()>,
    mut shutdown: Shutdown,
    hold_till_shutdown: mpsc::Sender<()>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CONFIG.sacct_frequency);
        loop {
            interval.tick().await;
            tokio::select! {
                records = get_job_info(&database) => {
                    match records {
                        Ok(records) => place_records_on_queue(records, &tx).await,
                        Err(e) => {
                            tracing::error!("something went wrong: {:?}", e);
                            continue
                        }
                    };
                },
                _ = shutdown.recv() => {
                    tracing::info!("Sacct monitor received shutdown signal. Shutting down.");
                    // shutdown properly
                    drop(hold_till_shutdown);
                    break
                },
            }
        }
    });
}

#[tracing::instrument(name = "Placing records on queue", level = "debug", skip(records, tx))]
async fn place_records_on_queue(records: Vec<RecordAdd>, tx: &mpsc::Sender<RecordAdd>) {
    for record in records {
        let record_id = record.record_id.clone();
        if let Err(e) = tx.send(record).await {
            tracing::error!("Could not send record {:?} to queue: {:?}", record_id, e);
        }
    }
}

#[tracing::instrument(name = "Calling sacct and parsing output", skip(database))]
async fn get_job_info(database: &Database) -> Result<Vec<RecordAdd>> {
    let lastcheck = database.get_lastcheck().await?;

    println!("Lastcheck: {}", lastcheck);

    let cmd_out = Command::new("/usr/bin/sacct")
        .arg("-a")
        .arg("--format")
        .arg(KEYS.iter().map(|k| k.0.clone()).join(","))
        .arg("--noconvert")
        .arg("--noheader")
        .arg("-S")
        .arg(format!(
            "{}",
            // todo: subtract a couple of seconds
            database.get_lastcheck().await?.format("%Y-%m-%dT%H:%M:%S")
        ))
        // .arg("now-1hours")
        .arg("-E")
        .arg("now")
        .arg("-s")
        .arg("completed,failed,node_fail")
        .arg("-P")
        .output()
        .await?;
    // .stdout;

    println!("stderr: {}", std::str::from_utf8(&cmd_out.stderr)?);
    println!("stdout: {}", std::str::from_utf8(&cmd_out.stdout)?);

    let sacct_rows = std::str::from_utf8(&cmd_out.stdout)?
        .lines()
        .map(|l| {
            println!("line: {}", l);
            KEYS.iter()
                .cloned()
                .zip(l.split('|').map(|s| s.to_owned()))
                // Occasionally fields are empty by design. filter those out to avoid
                // problems later on when parsing.
                .filter(|(_, v)| !v.is_empty())
                .map(|((k, pt), v)| {
                    let v = match pt.parse(&v) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            tracing::warn!(
                                "Parsing '{}' (key: {}) as {:?} failed: {:?}. This may or may not be a problem. It probably is.",
                                v,
                                k,
                                pt,
                                e
                            );
                            None
                        }
                    };
                    (k, v)
                })
                .collect::<HashMap<String, Option<AllowedTypes>>>()
        })
        .map(|hm| (hm["JobID"].as_ref().unwrap().extract_string().unwrap(), hm))
        .collect::<HashMap<String, HashMap<String,Option<AllowedTypes>>>>();

    println!("ROWs: {:?}", sacct_rows);

    let slurm_ids = sacct_rows
        .keys()
        .into_iter()
        .filter(|k| !BATCH_REGEX.is_match(k))
        .collect::<Vec<_>>();

    println!("SLURM IDs: {:?}", slurm_ids);

    let records = slurm_ids.into_iter().map(|id| -> Result<HashMap<String, AllowedTypes>> {
        let map1 = sacct_rows.get(id).ok_or(eyre!("Cannot get map1"))?;
        let map2 = sacct_rows.get(&format!("{}.batch", id)).expect("Cannot happen");
        KEYS.iter()
            .cloned()
            .map(|(k, _)| {
                let val =  match map1.get(&k) {
                    Some(Some(v)) => Ok(v.clone()),
                    _ => match map2.get(&k) {
                        Some(Some(v)) => Ok(v.clone()),
                        _ => {
                            tracing::error!("Something went wrong during parsing");
                            Err(eyre!("Something went wrong during parsing of sacct output. Can't recover."))
                        },
                    },
                }?;
                Ok((k, val))
            }).collect::<Result<HashMap<String, AllowedTypes>>>()
    }).collect::<Result<Vec<HashMap<String, AllowedTypes>>>>()?
    .iter()
    .map(|map| -> Result<RecordAdd> {
        Ok(
            RecordAdd::new(
                format!("{}-{}", make_string_valid(&CONFIG.record_prefix),
                map["JobID"].extract_string()?),
                make_string_valid(&CONFIG.site_id),
                make_string_valid(map["User"].extract_string()?),
                make_string_valid(map["Group"].extract_string()?),
                construct_components(&CONFIG, map),
                map["Start"].extract_datetime()?
            )
            .expect("Could not construct record")
            .with_stop_time(map["End"].extract_datetime()?)
        )
    }).collect::<Result<Vec<_>>>()?;

    let nextcheck = if records.is_empty() {
        lastcheck
    } else {
        let local_offset = Local::now().offset().utc_minus_local();
        println!("local_offset: {}", local_offset);
        let ts = records
            .iter()
            .fold(chrono::DateTime::<Utc>::MIN_UTC, |acc, r| {
                println!("timestamp: {}", r.stop_time.unwrap());
                acc.max(r.stop_time.unwrap())
            });
        DateTime::<Local>::from_utc(ts.naive_utc(), FixedOffset::east_opt(local_offset).unwrap())
    };

    println!("nextcheck: {}", nextcheck);

    database.set_lastcheck(nextcheck).await?;

    Ok(records)
}

#[tracing::instrument(name = "Remove forbidden characters from string", level = "debug")]
fn make_string_valid<T: AsRef<str> + fmt::Debug>(input: T) -> String {
    input.as_ref().replace(&FORBIDDEN_CHARACTERS[..], "")
}

#[tracing::instrument(
    name = "Construct components from job info and configuration",
    level = "debug"
)]
fn construct_components(config: &Settings, job: &Job) -> Vec<Component> {
    config
        .components
        .iter()
        .cloned()
        .filter(|c| {
            c.only_if.is_none() || {
                let only_if = c.only_if.as_ref().unwrap();
                let re = Regex::new(&only_if.matches)
                    .unwrap_or_else(|_| panic!("Invalid regex expression: {}", &only_if.matches));
                re.is_match(&job[&only_if.key].extract_string().unwrap_or_else(|_| {
                    panic!("Key is expectedto be a string: {:?}", job[&only_if.key])
                }))
            }
        })
        .map(|c| {
            Component::new(
                make_string_valid(c.name),
                job[&c.key].extract_i64().unwrap_or_else(|_| {
                    panic!(
                        "Cannot parse key {} (value: {:?}) into i64.",
                        c.key, job[&c.key]
                    )
                }),
            )
            .expect("Cannot construct component. Please check your configuration!")
            .with_scores(
                c.scores
                    .iter()
                    .filter(|s| {
                        s.only_if.is_none() || {
                            let only_if = s.only_if.as_ref().unwrap();
                            let re = Regex::new(&only_if.matches).unwrap_or_else(|_| {
                                panic!("Invalid regex expression: {}", &only_if.matches)
                            });
                            re.is_match(
                                &job[&only_if.key]
                                    .extract_string()
                                    .unwrap_or_else(|_| panic!("Error extracting string.")),
                            )
                        }
                    })
                    .map(|s| {
                        Score::new(s.name.clone(), s.factor)
                            .unwrap_or_else(|_| panic!("Cannot construct score from {:?}", s))
                    })
                    .collect(),
            )
        })
        .collect()
}
