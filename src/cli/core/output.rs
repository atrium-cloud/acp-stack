use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    Text,
    Json,
}

impl OutputFormat {
    pub(crate) fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OutputFormatChoice {
    format: OutputFormat,
    explicit: bool,
}

impl OutputFormatChoice {
    pub(crate) fn new(format: Option<OutputFormat>) -> Self {
        Self {
            format: format.unwrap_or(OutputFormat::Text),
            explicit: format.is_some(),
        }
    }

    pub(crate) fn effective(self) -> OutputFormat {
        self.format
    }

    pub(crate) fn reject_json(self, command: &'static str) -> Result<()> {
        if self.explicit && self.format == OutputFormat::Json {
            return Err(StackError::InvalidParam {
                field: "format",
                reason: format!("{command} does not support --format json"),
            });
        }
        Ok(())
    }

    pub(crate) fn resolve_json_alias(self, json: bool, flag: &'static str) -> Result<OutputFormat> {
        if json && self.explicit && self.format == OutputFormat::Text {
            return Err(StackError::InvalidParam {
                field: flag,
                reason: "--json conflicts with --format text; use --format json or omit --format"
                    .to_owned(),
            });
        }
        if json {
            Ok(OutputFormat::Json)
        } else {
            Ok(self.format)
        }
    }
}

pub(crate) fn print_json(data: &serde_json::Value) -> Result<()> {
    let rendered = serde_json::to_string_pretty(data).map_err(|source| StackError::ServeIo {
        source: std::io::Error::other(format!("serialize CLI JSON: {source}")),
    })?;
    println!("{rendered}");
    Ok(())
}

pub(crate) fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for ch in chars.by_ref() {
                let code = ch as u32;
                if (0x40..=0x7e).contains(&code) {
                    break;
                }
            }
        } else {
            output.push(c);
        }
    }
    output
}
