pub mod db;
pub mod roas_table;

pub use crate::db::*;
pub use crate::roas_table::*;

use crate::db::{roa_history, RoaFile};
use chrono::{Datelike, NaiveDate};
use ipnetwork::IpNetwork;
use rayon::prelude::*;
use regex::Regex;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::str::FromStr;
use tracing::{debug, info};

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct RoaEntry {
    tal: String,
    prefix: IpNetwork,
    max_len: i32,
    asn: u32,
    date: NaiveDate,
}

fn __crawl_years(tal_url: &str) -> Vec<String> {
    let year_pattern: Regex = Regex::new(r#"<a href=".*">\s*(\d\d\d\d)/</a>.*"#).unwrap();

    // get all years
    let body = reqwest::blocking::get(tal_url).unwrap().text().unwrap();
    let years: Vec<String> = year_pattern
        .captures_iter(body.as_str())
        .map(|cap| cap[1].to_owned())
        .collect();

    years
}

fn __crawl_months_days(months_days_url: &str) -> Vec<String> {
    let month_day_pattern: Regex = Regex::new(r#"<a href=".*">\s*(\d\d)/</a>.*"#).unwrap();

    let body = reqwest::blocking::get(months_days_url)
        .unwrap()
        .text()
        .unwrap();
    let months_days: Vec<String> = month_day_pattern
        .captures_iter(body.as_str())
        .map(|cap| cap[1].to_owned())
        .collect();

    months_days
}

/// Crawl and return all RIPE ROA file meta data after a given date
///
/// The ROA files URLs has the following format:
/// https://ftp.ripe.net/ripe/rpki/ripencc.tal/2022/08/28/roas.csv
pub fn crawl_tal_after(tal_url: &str, after: Option<NaiveDate>) -> Vec<RoaFile> {
    let fields: Vec<&str> = tal_url.split("/").collect();
    let tal = fields[4].split(".").collect::<Vec<&str>>()[0].to_owned();

    let min_date: NaiveDate = NaiveDate::from_ymd_opt(1000, 1, 1).unwrap();
    let after_date: NaiveDate = match after {
        None => min_date,
        Some(d) => d,
    };

    // get all years
    let years: Vec<i32> = __crawl_years(tal_url)
        .into_iter()
        .map(|y| y.parse::<i32>().unwrap())
        .filter(|y| {
            let date = NaiveDate::from_ymd_opt(*y, 1, 1).unwrap();
            date >= NaiveDate::from_ymd_opt(after_date.year(), 1, 1).unwrap()
        })
        .collect();

    years
        .par_iter()
        .map(|year| {
            info!("scraping data for {}/{} ...", &tal_url, &year);
            let year_url = format!("{}/{}", tal_url, year);

            let months: Vec<u32> = __crawl_months_days(year_url.as_str())
                .into_iter()
                .map(|m| m.parse::<u32>().unwrap())
                .filter(|m| {
                    let date = NaiveDate::from_ymd_opt(*year, *m, 1).unwrap();
                    date >= NaiveDate::from_ymd_opt(after_date.year(), after_date.month(), 1)
                        .unwrap()
                })
                .collect();

            months
                .par_iter()
                .map(|month| {
                    info!("scraping data for {}/{:02} ...", &year_url, &month);
                    let month_url = format!("{}/{:02}", year_url, month);

                    let days: Vec<u32> = __crawl_months_days(month_url.as_str())
                        .into_iter()
                        .map(|d| d.parse::<u32>().unwrap())
                        .filter(|d| {
                            let date = NaiveDate::from_ymd_opt(*year, *month, *d).unwrap();
                            date > after_date
                        })
                        .collect();

                    let roa_files = days
                        .into_iter()
                        .map(|day| {
                            let url = format!("{}/{:02}/roas.csv", month_url, day);
                            let file_date = NaiveDate::from_ymd_opt(*year, *month, day).unwrap();
                            RoaFile {
                                tal: tal.clone(),
                                url,
                                file_date,
                                rows_count: 0,
                                processed: false,
                            }
                        })
                        .collect::<Vec<RoaFile>>();

                    roa_files
                })
                .flat_map(|x| x)
                .collect::<Vec<RoaFile>>()
        })
        .flat_map(|x| x)
        .collect::<Vec<RoaFile>>()
}

pub fn crawl_tal(nic_url: &str, crawl_all: bool) -> Vec<RoaFile> {
    let fields: Vec<&str> = nic_url.split("/").collect();
    let tal = fields[4].split(".").collect::<Vec<&str>>()[0].to_owned();

    let month_day_pattern: Regex = Regex::new(r#"<a href=".*">\s*(\d\d)/</a>.*"#).unwrap();

    // get all years
    let years: Vec<String> = __crawl_years(nic_url);

    let roa_files = if crawl_all {
        years
            .par_iter()
            .map(|year| {
                info!("scraping data for {}/{} ...", &nic_url, &year);
                let year_url = format!("{}/{}", nic_url, year);

                let body = reqwest::blocking::get(year_url.as_str())
                    .unwrap()
                    .text()
                    .unwrap();
                let months: Vec<String> = month_day_pattern
                    .captures_iter(body.as_str())
                    .map(|cap| cap[1].to_owned())
                    .collect();

                months
                    .par_iter()
                    .map(|month| {
                        info!("scraping data for {}/{} ...", &year_url, &month);
                        let month_url = format!("{}/{}", year_url, month);
                        let body = reqwest::blocking::get(month_url.as_str())
                            .unwrap()
                            .text()
                            .unwrap();
                        let days: Vec<RoaFile> = month_day_pattern
                            .captures_iter(body.as_str())
                            .map(|cap| {
                                let day = cap[1].to_owned();
                                let url = format!("{}/{}/roas.csv", month_url, day);
                                let file_date = chrono::NaiveDate::from_ymd_opt(
                                    year.parse::<i32>().unwrap(),
                                    month.parse::<u32>().unwrap(),
                                    day.parse::<u32>().unwrap(),
                                )
                                .unwrap();
                                RoaFile {
                                    tal: tal.to_owned(),
                                    url,
                                    file_date,
                                    rows_count: 0,
                                    processed: false,
                                }
                            })
                            .collect();
                        days
                    })
                    .flat_map(|x| x)
                    .collect::<Vec<RoaFile>>()
            })
            .flat_map(|x| x)
            .collect::<Vec<RoaFile>>()
    } else {
        // get latest month's data
        // TODO: handle edge case where the latest month mismatch with local timezone
        let year = years.last().unwrap();
        let year_url = format!("{}/{}", nic_url, year);
        let body = reqwest::blocking::get(year_url.as_str())
            .unwrap()
            .text()
            .unwrap();
        let months: Vec<String> = month_day_pattern
            .captures_iter(body.as_str())
            .map(|cap| cap[1].to_owned())
            .collect();
        let month = months.last().unwrap();
        let month_url = format!("{}/{}", year_url, month);
        let body = reqwest::blocking::get(month_url.as_str())
            .unwrap()
            .text()
            .unwrap();

        // get each day
        month_day_pattern
            .captures_iter(body.as_str())
            .map(|cap| {
                let day = cap[1].to_owned();
                let url = format!("{}/{}/roas.csv", month_url, day);
                let file_date = chrono::NaiveDate::from_ymd_opt(
                    year.parse::<i32>().unwrap(),
                    month.parse::<u32>().unwrap(),
                    day.parse::<u32>().unwrap(),
                )
                .unwrap();
                RoaFile {
                    tal: tal.to_owned(),
                    url,
                    file_date,
                    rows_count: 0,
                    processed: false,
                }
            })
            .collect::<Vec<RoaFile>>()
    };

    roa_files
}

pub fn parse_roas_csv(csv_url: &str) -> HashSet<RoaEntry> {
    // parse csv url for auxiliary fields
    let fields: Vec<&str> = csv_url.split("/").collect();
    let tal = fields[4].split(".").collect::<Vec<&str>>()[0].to_owned();
    let year = fields[5].parse::<i32>().unwrap();
    let month = fields[6].parse::<u32>().unwrap();
    let day = fields[7].parse::<u32>().unwrap();
    let date = NaiveDate::from_ymd_opt(year, month, day).unwrap();

    let response = reqwest::blocking::get(csv_url).unwrap();
    let reader = BufReader::new(response);
    let mut roas = HashSet::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.starts_with("URI") {
            // skip the first line
            continue;
        }
        if line.to_lowercase().contains("html") {
            debug!("file {} does not exist, skipping", csv_url);
            break;
        }

        let fields = line.split(",").collect::<Vec<&str>>();
        let asn = fields[1]
            .strip_prefix("AS")
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let prefix = IpNetwork::from_str(fields[2].to_owned().as_str()).unwrap();
        let max_len = match fields[3].to_owned().parse::<i32>() {
            Ok(l) => l,
            Err(_e) => prefix.prefix() as i32,
        };

        roas.insert(RoaEntry {
            prefix,
            asn,
            max_len,
            tal: tal.to_owned(),
            date,
        });
    }

    roas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crawl() {
        let roa_files = crawl_tal("https://ftp.ripe.net/rpki/ripencc.tal", false);
        for x in roa_files {
            dbg!(x);
        }
    }

    #[test]
    fn test_parse() {
        let roas = parse_roas_csv("https://ftp.ripe.net/rpki/ripencc.tal/2022/01/15/roas.csv");
        for roa in &roas.iter().take(10).collect::<Vec<&RoaEntry>>() {
            println!("{} {} {}", roa.asn, roa.prefix, roa.max_len);
        }
    }

    #[test]
    fn test_crawl_after() {
        let after_date = NaiveDate::from_ymd_opt(2023, 3, 31).unwrap();
        let roa_files = crawl_tal_after("https://ftp.ripe.net/rpki/ripencc.tal", Some(after_date));
        assert!(roa_files.len() > 0);
        assert_eq!(
            roa_files[0].file_date,
            after_date + chrono::Duration::days(1)
        );
    }

    #[test]
    fn test_crawl_after_bootstrap() {
        let roa_files = crawl_tal_after("https://ftp.ripe.net/rpki/ripencc.tal", None);
        assert!(roa_files.len() > 0);
        assert_eq!(
            roa_files[0].file_date,
            NaiveDate::from_ymd_opt(2011, 1, 21).unwrap()
        );
    }
}
