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
#[serde(tag = "type")]
enum ToolchainSource {
    #[serde(rename = "ci")]
    Ci {
        sha: String,
        #[serde(rename = "try")]
        try_: bool,
    },
    #[serde(rename = "dist")]
    Dist { name: String },
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
    fn toolchain_name(&self, ty: ToolchainType) -> String {
        let idx = match ty {
            ToolchainType::Start => 0,
            ToolchainType::End => 1,
        };
        match &self.toolchains[idx].source {
            ToolchainSource::Ci { sha, try_ } => {
                format!("{}#{}", if *try_ { "try" } else { "master" }, sha)
            }
            ToolchainSource::Dist { name } => name.clone(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CcWho {
    All,
    Roots,
    None,
}

impl CcWho {
    fn causes(self) -> bool {
        match self {
            CcWho::All => true,
            CcWho::Roots | CcWho::None => false,
        }
    }

    fn roots(self) -> bool {
        match self {
            CcWho::All | CcWho::Roots => true,
            CcWho::None => false,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let experiment = args.get(1).unwrap_or_else(|| {
        eprintln!("Usage: {} <experiment name>", args[0]);
        std::process::exit(1);
    });
    let cc_ty = args.get(2).unwrap_or_else(|| {
        eprintln!(
            "Usage: {} <experiment name> <all|roots|none|print-list>",
            args[0]
        );
        std::process::exit(1);
    });
    let cc_ty = match cc_ty.as_str() {
        "all" => CcWho::All,
        "roots" => CcWho::Roots,
        "none" => CcWho::None,
        "print-list" => CcWho::None,
        _ => {
            eprintln!("Wrong second parameter: {:?}", cc_ty);
            eprintln!("Usage: {} <experiment name> <all|roots|none>", args[0]);
            std::process::exit(1);
        }
    };

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

    let compile_regex = Regex::new(r#"[Cc]ould not compile `([^)]+?)`"#).unwrap();
    let document_regex = Regex::new(r#"Could not document `([^`)]+?)`"#).unwrap();
    let mut rows = BTreeMap::new();
    let mut crate_list = String::new();
    for regression in regressions.values() {
        let end_log = regression.log(ToolchainType::End);
        {
            let id = match &regression.id {
                CrateId::CratesIo { package, .. } => package.clone(),
                CrateId::GitHub { user, repository } => format!("{}/{}", user, repository),
            };
            writeln!(&mut crate_list, "{}", id).unwrap();
        }
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
        let name = match &regression.id {
            CrateId::CratesIo { package, .. } => package.clone(),
            CrateId::GitHub { user, repository } => format!("{}/{}", user, repository),
        };
        if end_log.contains("error: test failed, to rerun pass '--lib'") {
            crates.push(SuspectedCause::TestFailure {
                crate_name: name.clone(),
            });
        }
        if end_log.contains("error: test failed, to rerun pass '--doc'") {
            crates.push(SuspectedCause::DocTestFailure {
                crate_name: name.clone(),
            });
        }
        if crates.len() == 1 {
            let cause = crates.pop().unwrap();

            rows.entry(cause).or_insert_with(Vec::new).push((
                &regression.id,
                "start",
                regression.log_url(&config, ToolchainType::Start),
                "end",
                regression.log_url(&config, ToolchainType::End),
                format_owners_to_cc(&regression.id.owners().expect(&format!("{}", regression.id))),
            ));
        } else {
            rows.entry(SuspectedCause::Unknown)
                .or_insert_with(Vec::new)
                .push((
                    &regression.id,
                    "start",
                    regression.log_url(&config, ToolchainType::Start),
                    "end",
                    regression.log_url(&config, ToolchainType::End),
                    format_owners_to_cc(
                        &regression.id.owners().expect(&format!("{}", regression.id)),
                    ),
                ));
        }
    }
    std::fs::write("crate-list.txt", crate_list.trim_end_matches(",")).unwrap();

    let mut table = String::new();
    for (cause, affected) in rows {
        if affected.len() == 1 {
            let row = &affected[0];
            writeln!(
                table,
                " * root: {}: [{}]({}) v. [{}]({}){}",
                row.0,
                row.1,
                row.2,
                row.3,
                row.4,
                if cc_ty.roots() {
                    format!("; cc {}", row.5)
                } else {
                    String::new()
                }
            )
            .unwrap();
        } else {
            writeln!(
                table,
                "\nroot: {} - {} detected crates which regressed due to this{}",
                cause,
                affected.len(),
                if cc_ty.roots() {
                    match cause
                        .crate_name()
                        .and_then(|n| owners_for_crate_name(&n).ok())
                    {
                        Some(v) => format!("; cc {}", format_owners_to_cc(&v)),
                        None => format!("no owner?"),
                    }
                } else {
                    String::new()
                }
            )
            .unwrap();
            writeln!(table, "<details>\n").unwrap();
            for row in affected {
                let author = if cause == SuspectedCause::Unknown {
                    row.5
                } else {
                    format!("`{}`", row.5)
                };
                writeln!(
                    table,
                    " * {}: [{}]({}) v. [{}]({}){}",
                    row.0,
                    row.1,
                    row.2,
                    row.3,
                    row.4,
                    if cc_ty.causes() {
                        format!("; cc {}", author)
                    } else {
                        String::new()
                    }
                )
                .unwrap();
            }
            writeln!(table, "\n</details>\n").unwrap();
        }
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
    TestFailure { crate_name: String },
    DocTestFailure { crate_name: String },
    Unknown,
}

impl fmt::Display for SuspectedCause {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let name = match self {
            SuspectedCause::CompileError { crate_name } => crate_name.as_str(),
            SuspectedCause::DocumentaionError { crate_name } => crate_name.as_str(),
            SuspectedCause::TestFailure { crate_name } => crate_name.as_str(),
            SuspectedCause::DocTestFailure { crate_name } => crate_name.as_str(),
            SuspectedCause::Unknown => return write!(f, "unknown causes"),
        };
        write!(f, "{}", name)
    }
}

impl SuspectedCause {
    fn crate_name(&self) -> Option<&str> {
        match self {
            SuspectedCause::CompileError { crate_name } => Some(crate_name.as_str()),
            SuspectedCause::DocumentaionError { crate_name } => Some(crate_name.as_str()),
            SuspectedCause::TestFailure { crate_name } => Some(crate_name.as_str()),
            SuspectedCause::DocTestFailure { crate_name } => Some(crate_name.as_str()),
            SuspectedCause::Unknown => None,
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
