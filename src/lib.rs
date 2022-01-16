use std::io::{BufRead, BufReader};
use regex::Regex;
use tracing::{debug, error, info, span, warn, Level};
use rayon::prelude::*;

fn crawl_nic(nic_url: &str, crawl_all: bool) {
    let year_pattern: Regex = Regex::new(r#"<a href=".*"> (....)/</a>.*"#).unwrap();
    let month_day_pattern: Regex = Regex::new(r#"<a href=".*"> (..)/</a>.*"#).unwrap();

    // get all years
    let body = reqwest::blocking::get(nic_url).unwrap().text().unwrap();
    let mut years: Vec<String> = year_pattern.captures_iter(body.as_str()).map(|cap|{
        cap[1].to_owned()
    }).collect();

    if crawl_all {
        let roa_links = years.par_iter().map(|year| {
            info!("scraping data for {}/{} ...", &nic_url, &year);
            let year_url = format!("{}/{}", nic_url, year);

            let body = reqwest::blocking::get(year_url.as_str()).unwrap().text().unwrap();
            let mut months: Vec<String> = month_day_pattern.captures_iter(body.as_str()).map(|cap|{
                cap[1].to_owned()
            }).collect();

            months.par_iter().map(|month| {
                info!("scraping data for {}/{} ...", &year_url, &month);
                let month_url = format!("{}/{}", year_url, month);
                let body = reqwest::blocking::get(month_url.as_str()).unwrap().text().unwrap();
                let days: Vec<String> = month_day_pattern.captures_iter(body.as_str()).map(|cap|{
                    format!("{}/{}/roas.csv", month_url, cap[1].to_owned())
                }).collect();
                days
            }).flat_map(|x|x).collect::<Vec<String>>()
        }).flat_map(|x|x).collect::<Vec<String>>();

        for link in &roa_links {
            info!("{}", link);
        }
    } else {
        let year = years.last().unwrap();
        let year_url = format!("{}/{}", nic_url, year);
        let body = reqwest::blocking::get(year_url.as_str()).unwrap().text().unwrap();
        let mut months: Vec<String> = month_day_pattern.captures_iter(body.as_str()).map(|cap|{
            cap[1].to_owned()
        }).collect();
        let month = months.last().unwrap();
        let month_url = format!("{}/{}", year_url, month);
        let body = reqwest::blocking::get(month_url.as_str()).unwrap().text().unwrap();

        let roa_links: Vec<String> = month_day_pattern.captures_iter(body.as_str()).map(|cap|{
            format!("{}/{}/roas.csv", month_url, cap[1].to_owned())
        }).collect();

        for link in &roa_links {
            info!("{}", link);
        }
    }
}

struct ROA {
    prefix: String,
    asn: u32,
}

fn parse_roas_csv(csv_url: &str) -> Vec<ROA> {
    let response = reqwest::blocking::get(csv_url).unwrap();
    let reader = BufReader::new(response);
    let mut roas = vec![];
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break
        };
        if line.starts_with("URI") {
            // skip the first line
            continue
        }

        let fields = line.split(",").collect::<Vec<&str>>();
        let asn = fields[1].strip_prefix("AS").unwrap().parse::<u32>().unwrap();
        let prefix = fields[2].to_owned();
        roas.push(ROA{prefix, asn})
    }

    roas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crawl() {

        tracing_subscriber::fmt() .with_max_level(Level::INFO) .init();
        crawl_nic("https://ftp.ripe.net/rpki/ripencc.tal", false);
    }

    #[test]
    fn test_parse() {
        tracing_subscriber::fmt() .with_max_level(Level::INFO) .init();
        let roas = parse_roas_csv("https://ftp.ripe.net/rpki/ripencc.tal/2022/01/15/roas.csv");
        for roa in &roas[..10] {
            println!("{} {}", roa.asn, roa.prefix);
        }
    }
}
