use clap::ArgMatches;
use reqwest::multipart::{Form, Part};
use std::fs;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;
use regex::Regex;
use colored::Colorize;
use scraper::{Html, Selector};
use selectors::attr::CaseSensitivity;
use crate::StdErr;
use crate::problem::Problem;
use crate::config::{Config, Credentials};
use crate::kattis_client::KattisClient;

const CHECKBOX: &'static str = "\u{2705}"; // Green checkbox emoji
const CROSSMARK: &'static str = "\u{274C}"; // Red X emoji
const SLEEP_DURATION: Duration = Duration::from_secs(1);

pub async fn submit(cmd: &ArgMatches<'_>) -> Result<(), StdErr> {
    let problem = Problem::from_args(cmd)?;

    let file_path = problem.file();
    let file_name = match file_path.file_name() {
        Some(f) => f,
        None => return Err("failed to get file name".into()),
    }.to_str().expect("file path contained invalid unicode");

    if !cmd.is_present("yes") {
        println!("{}:  {}", "Problem".bright_cyan(), problem.name());
        println!("{}: {}", "Language".bright_cyan(), problem.lang());
        println!("{}:     {}", "File".bright_cyan(), file_name);
        print!("Is this correct? (y/n): ");
        io::stdout().flush().expect("failed to flush stdout");

        let mut input = String::new();
        if let Err(_) = io::stdin().read_line(&mut input) {
            return Err("failed to read input".into());
        }

        if input.trim().to_lowercase() != "y" {
            return Ok(());
        }
    }

    let cfg = Config::load()?;
    let creds = cfg.get_credentials()?;
    let submit_url = cfg.get_submit_url()?;
    let login_url = cfg.get_login_url()?;

    let client = KattisClient::new()?;
    client.login(creds.clone(), login_url).await?;

    let id = match submit_problem(&client, &problem, submit_url).await? {
        Some(i) => i,
        None => return Err("something went wrong during submission".into()),
    };

    let submission_url = format!("{}/{}", cfg.get_submissions_url()?, &id);

    println!("{} solution to {}", "submitted".bright_green(), &submission_url.underline());

    show_submission_status(&client, creds, &submission_url, login_url).await?;

    Ok(())
}

async fn submit_problem(kc: &KattisClient, problem: &Problem, submit_url: &str) -> Result<Option<String>, StdErr> {
    let file_path = problem.file();
    let file_name = file_path.file_name().unwrap().to_str().unwrap().to_string();

    let file_bytes = match fs::read(&file_path) {
        Ok(b) => b,
        Err(_) => return Err("failed to read solution file".into())
    };
    let file_part = Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str("application/octet-stream")
        .expect("failed to set mime type for file");

    let form = Form::new()
        .text("problem", problem.name())
        .text("language", problem.lang().to_string())
        .text("mainclass", problem.get_main_class().unwrap_or(String::new()))
        .part("sub_file[]", file_part)
        .text("submit_ctr", "2")
        .text("submit", "true")
        .text("script", "true");

    let res = kc.client.post(submit_url)
        .multipart(form)
        .send()
        .await?;

    let status = res.status();
    if !status.is_success() {
        return Err(format!("failed to submit to kattis (http status code {})", status).into());
    }

    let content = match res.text().await {
        Ok(t) => t,
        Err(_) => return Err("failed to read response from kattis".into()),
    };

    if content.contains("Problem not found") {
        return Err(format!("the problem \"{}\" does not exist", problem.name()).into());
    }

    let re = Regex::new(r"ID: (\d+)").unwrap();
    let id = re.captures(&content)
        .and_then(|c| c.get(1))
        .and_then(|i| Some(i.as_str().to_string()));

    Ok(id)
}

#[derive(Debug, Clone)]
enum TestCase {
    Accepted,
    Rejected(String),
    Unfinished,
}

async fn show_submission_status(kc: &KattisClient, creds: Credentials, submission_url: &str, login_url: &str) -> Result<(), StdErr> {
    let fail_reason_re = Regex::new(r"([\w ]+)$").unwrap();
    let mut fail = None;
    let mut num_passed;
    let mut num_failed;

    loop {
        // For some odd and godforsaken reason, we must log in before every request.
        kc.login(creds.clone(), login_url).await?;
        let res = kc.client.get(submission_url).send().await?;

        let status = res.status();
        if !status.is_success() {
            return Err(format!("failed to fetch submission progress (http status code {})", status).into());
        }

        let html = match res.text().await {
            Ok(h) => h,
            Err(_) => return Err("failed to read submission progress response from kattis".into()),
        };

        let doc = Html::parse_document(&html);

        let status_selector = Selector::parse("td.status").unwrap();
        let status_el = match doc.select(&status_selector).next() {
            Some(s) => s,
            None => return Err("failed to read submission status from kattis".into()),
        };
        let status = status_el.text().collect::<String>().to_lowercase();

        if status.contains("compile error") {
            print!("\r");
            io::stdout().flush().expect("failed to flush stdout");

            return Err("kattis could not compile your code".into());
        }

        if status.contains("new") || status.contains("compiling") {
            print!("\r{}: {}", "status".bright_cyan(), &status);
            io::stdout().flush().expect("failed to flush stdout");

            thread::sleep(SLEEP_DURATION);
            continue;
        }

        let test_selector = Selector::parse(".testcases > span").unwrap();
        let mut tests = Vec::new();
        num_passed = 0;
        num_failed = 0;

        for test_sel in doc.select(&test_selector) {
            let test_el = test_sel.value();
            let cs = CaseSensitivity::AsciiCaseInsensitive;
            let test = if test_el.has_class("accepted", cs) {
                num_passed += 1;
                TestCase::Accepted
            } else if test_el.has_class("rejected", cs) {
                num_failed += 1;

                let reason = test_el.attr("title")
                    .and_then(|t| fail_reason_re.captures(t))
                    .and_then(|c| c.get(1))
                    .and_then(|i| Some(i.as_str().trim().to_lowercase()))
                    .unwrap_or(String::from("unknown"));
                let rej = TestCase::Rejected(reason);

                // We only show the first failure reason
                if let None = fail {
                    fail = Some(rej.clone());
                }

                rej
            } else {
                TestCase::Unfinished
            };

            tests.push(test);
        }

        print!("\rRunning tests ... {} of {}: ", num_passed + num_failed, tests.len());

        for test in &tests {
            let symbol = match test {
                TestCase::Accepted => CHECKBOX,
                TestCase::Rejected(_) => CROSSMARK,
                TestCase::Unfinished => continue,
            };

            print!("{}", symbol);
        }
        io::stdout().flush().expect("failed to flush stdout");

        if let Some(_) = fail {
            break;
        }

        if num_passed + num_failed == tests.len() {
            break;
        }

        thread::sleep(SLEEP_DURATION);
    }

    let result_str = if let Some(_) = fail { "failed".bright_red() } else { "ok".bright_green() };
    let suffix = fail.and_then(|f| match f {
        TestCase::Rejected(r) => Some(format!("\nreason: {}.", r.bright_red())),
        _ => None,
    }).unwrap_or(String::new());

    println!("\n\nsubmission result: {}. {} passed; {} failed.{}", result_str, num_passed, num_failed, suffix);

    Ok(())
}
