use flate2::read::GzDecoder;
use regex::Regex;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::BufReader;
use std::io::{Read, Write as _};

lazy_static::lazy_static! {
    static ref CLIENT: reqwest::Client = reqwest::Client::new();
}

percent_encoding::define_encode_set! {
    pub REPORT_ENCODE_SET = [percent_encoding::DEFAULT_ENCODE_SET] | { '+' }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum CrateId {
    CratesIo { package: String, version: String },
    GitHub { user: String, repository: String },
}

#[derive(serde::Deserialize, Debug)]
struct CratesIoOwners {
    users: Vec<CratesIoUser>,
}

#[derive(serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum CratesIoUserKind {
    User,
    Team,
}

#[derive(serde::Deserialize, Debug)]
struct CratesIoUser {
    kind: CratesIoUserKind,
    login: String,
    url: String,
}

impl CratesIoUser {
    fn gh_username(&self) -> Option<&str> {
        let prefix = "https://github.com/";
        if self.url.starts_with(prefix) && self.kind == CratesIoUserKind::User {
            Some(&self.login)
        } else {
            None
        }
    }
}

fn owners_for_crate_name(package: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let owners: CratesIoOwners = CLIENT
        .get(&format!(
            "https://crates.io/api/v1/crates/{}/owners",
            package
        ))
        .header(reqwest::header::USER_AGENT, "crater-generate-report")
        .send()
        .unwrap()
        .json()?;

    Ok(owners
        .users
        .into_iter()
        .flat_map(|u| u.gh_username().map(String::from))
        .collect())
}

fn format_owners_to_cc(owners: &[String]) -> String {
    owners
        .into_iter()
        .map(|o| format!("@{}", o))
        .collect::<Vec<_>>()
        .join(", ")
}

impl CrateId {
    fn owners(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        match self {
            CrateId::CratesIo { package, .. } => owners_for_crate_name(&package),
            CrateId::GitHub { user, .. } => Ok(vec![user.into()]),
        }
    }
}

impl fmt::Display for CrateId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CrateId::GitHub { user, repository } => write!(f, "{}/{}", user, repository),
            CrateId::CratesIo { package, version } => write!(f, "{}-{}", package, version),
        }
    }
}

#[derive(serde::Deserialize, Debug)]
struct ToolchainSource {
    name: String,
}

#[derive(serde::Deserialize, Debug)]
struct Toolchain {
    source: ToolchainSource,
}

#[derive(serde::Deserialize, Debug)]
struct Config {
    name: String,
    toolchains: Vec<Toolchain>,
}

impl Config {
    fn toolchain_name(&self, ty: ToolchainType) -> &str {
        let idx = match ty {
            ToolchainType::Start => 0,
            ToolchainType::End => 1,
        };
        &self.toolchains[idx].source.name
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let experiment = args.get(1).unwrap_or_else(|| {
        eprintln!("Usage: {} <experiment name>", args[0]);
        std::process::exit(1);
    });

    let url = format!(
        "https://crater-reports.s3.amazonaws.com/{}/config.json",
        experiment
    );
    let config: Config = reqwest::get(&url)
        .unwrap_or_else(|e| {
            eprintln!("failed to get {:?}: {:?}", url, e);
            std::process::exit(1);
        })
        .json()
        .unwrap_or_else(|e| {
            eprintln!("failed to deserialize response from {:?}: {:?}", url, e);
            std::process::exit(1);
        });

    let url = format!(
        "https://crater-reports.s3.amazonaws.com/{}/logs-archives/regressed.tar.gz",
        experiment
    );
    let res = reqwest::get(&url).unwrap_or_else(|e| {
        eprintln!("failed to download regressed logs from {:?}: {:?}", url, e);
        std::process::exit(1);
    });
    let mut tarball = tar::Archive::new(GzDecoder::new(BufReader::new(res)));
    let mut regressions = BTreeMap::new();
    for entry in tarball.entries()? {
        let mut entry = entry?;

        let mut log = String::new();
        entry.read_to_string(&mut log)?;

        let path = entry.path()?;
        let name = path
            .components()
            .nth(2)
            .unwrap()
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let version_or_repo = path
            .components()
            .nth(3)
            .unwrap()
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let toolchain = path
            .components()
            .nth(4)
            .unwrap()
            .as_os_str()
            .to_string_lossy()
            .replace(".txt", "");

        let res = match path.components().nth(1).unwrap() {
            std::path::Component::Normal(v) => {
                if v == "gh" {
                    CrateId::GitHub {
                        user: name,
                        repository: version_or_repo,
                    }
                } else if v == "reg" {
                    CrateId::CratesIo {
                        package: name,
                        version: version_or_repo,
                    }
                } else {
                    panic!("unexpected component: {:?}", v);
                }
            }
            c => panic!("unexpected component: {:?}", c),
        };
        regressions
            .entry(res.clone())
            .or_insert_with(|| Regression::new(res))
            .insert(&config, &toolchain, log);
    }

    let compile_regex = Regex::new(r#"Could not compile `([^)]+?)`"#).unwrap();
    let document_regex = Regex::new(r#"Could not document `([^`)]+?)`"#).unwrap();
    let mut rows = BTreeMap::new();
    for regression in regressions.values() {
        let end_log = regression.log(ToolchainType::End);
        let mut crates = Vec::new();
        for capture in compile_regex.captures_iter(&end_log) {
            crates.push(SuspectedCause::CompileError {
                crate_name: capture[1].into(),
            });
        }
        for capture in document_regex.captures_iter(&end_log) {
            crates.push(SuspectedCause::DocumentaionError {
                crate_name: capture[1].into(),
            });
        }
        assert_eq!(crates.len(), 1, "{:?}: {:?}", regression.id, crates);
        let cause = crates.pop().unwrap();

        rows.entry(cause).or_insert_with(Vec::new).push((
            &regression.id,
            "start",
            regression.log_url(&config, ToolchainType::Start),
            "end",
            regression.log_url(&config, ToolchainType::End),
            format_owners_to_cc(&regression.id.owners().expect(&format!("{}", regression.id))),
        ));
    }
    let mut table = String::new();
    for (cause, affected) in rows {
        writeln!(
            table,
            "\nroot: {} - {} detected crates which regressed due to this; {}",
            cause.crate_name(),
            affected.len(),
            match owners_for_crate_name(&cause.crate_name()) {
                Ok(v) => format!("cc {}", format_owners_to_cc(&v)),
                Err(_) => format!("no owner?"),
            }
        )
        .unwrap();
        writeln!(table, "<details>\n").unwrap();
        for row in affected {
            writeln!(
                table,
                " * {}: [{}]({}) v. [{}]({}); cc `{}`",
                row.0, row.1, row.2, row.3, row.4, row.5
            )
            .unwrap();
        }
        writeln!(table, "\n</details>\n").unwrap();
    }
    std::io::stdout().write_all(table.as_bytes()).unwrap();

    Ok(())
}

#[derive(Debug, Clone)]
struct Regression {
    id: CrateId,
    start_log: Option<String>,
    end_log: Option<String>,
}

#[derive(Copy, Clone, Debug)]
enum ToolchainType {
    Start,
    End,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SuspectedCause {
    CompileError { crate_name: String },
    DocumentaionError { crate_name: String },
}

impl SuspectedCause {
    fn crate_name(&self) -> &str {
        match self {
            SuspectedCause::CompileError { crate_name } => crate_name.as_str(),
            SuspectedCause::DocumentaionError { crate_name } => crate_name.as_str(),
        }
    }
}

impl Regression {
    fn new(id: CrateId) -> Self {
        Regression {
            id,
            start_log: None,
            end_log: None,
        }
    }

    fn log(&self, ty: ToolchainType) -> &str {
        match ty {
            ToolchainType::Start => self.start_log.as_ref().map(|s| s.as_str()).unwrap(),
            ToolchainType::End => self.end_log.as_ref().map(|s| s.as_str()).unwrap(),
        }
    }

    fn log_url(&self, cfg: &Config, ty: ToolchainType) -> String {
        percent_encoding::utf8_percent_encode(
            &format!(
                "https://crater-reports.s3.amazonaws.com/{}/{}/{}/log.txt",
                cfg.name,
                cfg.toolchain_name(ty),
                match &self.id {
                    CrateId::GitHub { user, repository } => format!("gh/{}.{}", user, repository),
                    CrateId::CratesIo { package, version } => {
                        format!("reg/{}-{}", package, version)
                    }
                }
            ),
            REPORT_ENCODE_SET,
        )
        .to_string()
    }

    fn insert(&mut self, cfg: &Config, toolchain: &str, log: String) {
        if cfg.toolchain_name(ToolchainType::Start) == toolchain {
            assert!(
                self.start_log.is_none(),
                "replacing existing start log for {:?}",
                self.id
            );
            self.start_log = Some(log);
        } else if cfg.toolchain_name(ToolchainType::End) == toolchain {
            assert!(
                self.end_log.is_none(),
                "replacing existing start log for {:?}",
                self.id
            );
            self.end_log = Some(log);
        } else {
            panic!(
                "unknown toolchain: {:?}, valid options: {:?} or {:?}",
                toolchain,
                cfg.toolchain_name(ToolchainType::Start),
                cfg.toolchain_name(ToolchainType::End),
            );
        }
    }
}
