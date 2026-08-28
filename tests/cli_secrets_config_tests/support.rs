use crate::common::cli::*;

fn parse_key_line(stdout: &str, label: &'static str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .unwrap_or_else(|| panic!("missing {label} in stdout: {stdout}"))
        .trim()
        .to_owned()
}

fn parse_init_keys(stdout: &str) -> (String, String) {
    (
        parse_key_line(stdout, "session key: "),
        parse_key_line(stdout, "admin key: "),
    )
}

pub(crate) fn run_init_with_home(home: &std::path::Path) -> (String, String) {
    let stdout = acps_command(home)
        .args(["dev", "init", "--agent", "placebo", "--skip-workspace-init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(stdout).expect("init stdout utf8");
    parse_init_keys(&stdout)
}
