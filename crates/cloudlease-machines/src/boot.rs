//! Validating the client's boot data and rendering the droplet's user data
//!
//! The user data is a POSIX shell script that cloud-init runs once as root. In order, it:
//!
//! 1. writes `/etc/cloudlease/lease.json` (mode 0600): the lease ID, its token, the API base,
//!    the hard deadline, the heartbeat TTL and, for a provider that can take the machine back,
//!    where the machine hears that it is about to
//! 2. formats and mounts the class's volume, when the class has one
//! 3. writes each client file, then runs each client command, stopping at the first failure
//!
//! Everything goes to `/var/log/cloudlease-boot.log`. File contents travel base64-encoded and
//! every path and argument is single-quoted, so nothing the client sends is parsed by the
//! shell. User data stays readable through the metadata service, which is why boot data may
//! hold nothing secret beyond the lease token, which can only renew or release its own lease.

use std::fmt::Write as _;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cloudlease_core::time::UnixMillis;
use cloudlease_provider::digitalocean::{BootScript, ResourceName};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::time::Timestamp;

/// Where the machine finds its lease and lease token
pub const LEASE_FILE: &str = "/etc/cloudlease/lease.json";

/// Where the boot script logs
pub const BOOT_LOG: &str = "/var/log/cloudlease-boot.log";

/// The largest `boot` object, as JSON
pub const MAX_BOOT_BYTES: usize = 32 * 1024;

/// The most files a `boot` object may write
pub const MAX_BOOT_FILES: usize = 16;

/// The most commands a `boot` object may run
pub const MAX_BOOT_RUNS: usize = 8;

const MAX_PATH_BYTES: usize = 255;

/// Boot data the API refuses with `400 invalid_boot`
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BootError {
    /// The `boot` object is too large
    #[error("boot is {0} bytes as JSON; at most 32 KiB")]
    TooLarge(usize),
    /// Too many files
    #[error("boot writes {0} files; at most 16")]
    TooManyFiles(usize),
    /// Too many commands
    #[error("boot runs {0} commands; at most 8")]
    TooManyRuns(usize),
    /// A path is not a plain absolute path
    #[error(
        "{0:?} must be an absolute path of letters, digits and . _ - + @, with no . or .. parts"
    )]
    InvalidPath(String),
    /// A mode is not an octal permission
    #[error("{0:?} must be an octal mode such as \"0600\"")]
    InvalidMode(String),
    /// A command has no program
    #[error("command {0} is empty")]
    EmptyCommand(usize),
    /// An argument holds a NUL byte, which no argv can carry
    #[error("command {0} has an argument with a NUL byte")]
    NulArgument(usize),
    /// The rendered user data passes the provider's limit
    #[error("the rendered user data is too large for the provider")]
    UserDataTooLarge,
}

/// One file the boot script writes
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootFile {
    /// An absolute path
    pub path: String,
    /// An octal mode, such as `0600`
    pub mode: String,
    /// UTF-8 content
    pub content: String,
}

/// The `boot` object of a machine create, as the client sent it
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootPayload {
    /// Files to write, in order
    pub files: Vec<BootFile>,
    /// Commands to run in order as root, each an argv
    pub run: Vec<Vec<String>>,
}

/// Boot data that passed every check
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Boot(BootPayload);

impl Boot {
    /// Checks the contract's limits and that every path and mode is plain
    pub fn new(payload: BootPayload) -> Result<Self, BootError> {
        let size = serde_json::to_vec(&payload).map_or(usize::MAX, |json| json.len());
        if size > MAX_BOOT_BYTES {
            return Err(BootError::TooLarge(size));
        }
        if payload.files.len() > MAX_BOOT_FILES {
            return Err(BootError::TooManyFiles(payload.files.len()));
        }
        if payload.run.len() > MAX_BOOT_RUNS {
            return Err(BootError::TooManyRuns(payload.run.len()));
        }
        for file in &payload.files {
            check_path(&file.path)?;
            check_mode(&file.mode)?;
        }
        for (index, command) in payload.run.iter().enumerate() {
            if command.first().is_none_or(String::is_empty) {
                return Err(BootError::EmptyCommand(index));
            }
            if command.iter().any(|argument| argument.contains('\0')) {
                return Err(BootError::NulArgument(index));
            }
        }

        Ok(Self(payload))
    }

    /// Returns the payload
    #[must_use]
    pub const fn payload(&self) -> &BootPayload {
        &self.0
    }
}

/// An absolute path the boot script may write or mount at
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountPath(String);

impl MountPath {
    /// Validates a path
    pub fn parse(value: &str) -> Result<Self, BootError> {
        check_path(value)?;

        Ok(Self(value.to_owned()))
    }

    /// Returns the path
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn check_path(path: &str) -> Result<(), BootError> {
    let invalid = || BootError::InvalidPath(path.to_owned());
    let Some(relative) = path.strip_prefix('/') else {
        return Err(invalid());
    };
    if path.len() > MAX_PATH_BYTES {
        return Err(invalid());
    }
    let plain = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(character, '.' | '_' | '-' | '+' | '@')
            })
    };
    if !relative.split('/').all(plain) {
        return Err(invalid());
    }

    Ok(())
}

fn check_mode(mode: &str) -> Result<(), BootError> {
    let octal = (3..=4).contains(&mode.len()) && mode.chars().all(|c| matches!(c, '0'..='7'));
    if !octal {
        return Err(BootError::InvalidMode(mode.to_owned()));
    }

    Ok(())
}

/// Where a machine hears that its provider is about to take it back
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionNotice {
    /// The EC2 instance metadata service, which posts a Spot interruption notice about two
    /// minutes before EC2 reclaims the instance
    AwsImds,
}

/// The machine's `lease.json`
#[derive(Clone, Debug, Serialize)]
pub struct LeaseFile<'a> {
    /// The lease
    pub lease_id: &'a str,
    /// The lease token, which can only renew or release this lease
    pub token: &'a str,
    /// The API base URL
    pub api_base: &'a str,
    /// When the lease ends whatever happens
    pub hard_deadline_at: Timestamp,
    /// How long a renew keeps the lease alive
    pub heartbeat_ttl_secs: u64,
    /// Where the machine hears of an interruption, absent for a provider that never takes a
    /// leased machine back
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interruption_notice: Option<InterruptionNotice>,
}

/// The class volume the boot script formats and mounts
#[derive(Clone, Copy, Debug)]
pub struct VolumeMount<'a> {
    /// The volume's name, which names its device
    pub name: &'a ResourceName,
    /// Where to mount it
    pub mount: &'a MountPath,
}

/// Renders the droplet's user data
pub fn render_user_data(
    lease: &LeaseFile<'_>,
    volume: Option<VolumeMount<'_>>,
    boot: &Boot,
) -> Result<BootScript, BootError> {
    let lease_json = serde_json::to_string(lease).map_err(|_| BootError::UserDataTooLarge)?;
    let mut script = String::new();

    // writes go through a function so each file is created under a 077 umask, never briefly
    // readable by others before its chmod
    let _ = write!(
        script,
        "#!/bin/sh\n\
         # written by cloudlease from the machine's lease and its client's boot data\n\
         set -eu\n\
         exec >>{log} 2>&1\n\
         echo \"cloudlease boot: start $(date -u +%Y-%m-%dT%H:%M:%SZ)\"\n\
         cloudlease_write() {{\n\
         \tmkdir -p \"$(dirname \"$1\")\"\n\
         \t(umask 077 && printf '%s' \"$3\" | base64 -d >\"$1\")\n\
         \tchmod \"$2\" \"$1\"\n\
         }}\n\
         cloudlease_write {lease_file} 0600 {lease}\n",
        log = quote(BOOT_LOG),
        lease_file = quote(LEASE_FILE),
        lease = quote(&STANDARD.encode(lease_json)),
    );

    if let Some(volume) = volume {
        let device = format!("/dev/disk/by-id/scsi-0DO_Volume_{}", volume.name.as_str());
        let _ = write!(
            script,
            "cloudlease_volume={device}\n\
             cloudlease_wait=0\n\
             while [ ! -e \"$cloudlease_volume\" ] && [ \"$cloudlease_wait\" -lt 60 ]; do\n\
             \tsleep 2\n\
             \tcloudlease_wait=$((cloudlease_wait + 1))\n\
             done\n\
             blkid \"$cloudlease_volume\" >/dev/null 2>&1 || mkfs.ext4 -q -F \"$cloudlease_volume\"\n\
             mkdir -p {mount}\n\
             mount -o defaults,discard,noatime \"$cloudlease_volume\" {mount}\n",
            device = quote(&device),
            mount = quote(volume.mount.as_str()),
        );
    }

    for file in &boot.payload().files {
        let _ = writeln!(
            script,
            "cloudlease_write {} {} {}",
            quote(&file.path),
            quote(&file.mode),
            quote(&STANDARD.encode(&file.content)),
        );
    }
    for (index, command) in boot.payload().run.iter().enumerate() {
        let argv: Vec<String> = command.iter().map(|argument| quote(argument)).collect();
        let _ = write!(
            script,
            "echo 'cloudlease boot: command {number}'\n{argv}\n",
            number = index + 1,
            argv = argv.join(" "),
        );
    }
    script.push_str("echo 'cloudlease boot: done'\n");

    BootScript::parse(script).map_err(|_| BootError::UserDataTooLarge)
}

/// Checks that boot data renders within the user data limit for any lease, and returns the
/// largest script it can render
///
/// It renders with the longest lease ID, token, deadline and volume name a run can produce,
/// so boot data too large once rendered is refused before a machine is admitted or anything
/// is created. A provider that wraps the script checks the result again with
/// [`ProviderClass::check_user_data`](crate::config::ProviderClass::check_user_data).
pub fn check_fits(
    boot: &Boot,
    api_base: &str,
    mount: Option<&MountPath>,
) -> Result<BootScript, BootError> {
    let longest_run = format!("run_{}", "f".repeat(24));
    let lease_id = format!("{longest_run}-{}", u64::MAX);
    let token = format!("cll_{}", "f".repeat(64));
    let volume = ResourceName::parse(format!("{}-{}", longest_run.replace('_', "-"), u64::MAX))
        .map_err(|_| BootError::UserDataTooLarge)?;
    // 9999-12-31T23:59:59.999Z, the longest deadline the timestamp format writes
    let latest = UnixMillis::new(253_402_300_799_999).ok_or(BootError::UserDataTooLarge)?;
    let lease = LeaseFile {
        lease_id: &lease_id,
        token: &token,
        api_base,
        hard_deadline_at: Timestamp(latest),
        heartbeat_ttl_secs: u64::MAX,
        // the longest lease file, whatever the provider
        interruption_notice: Some(InterruptionNotice::AwsImds),
    };
    let volume = mount.map(|mount| VolumeMount {
        name: &volume,
        mount,
    });

    render_user_data(&lease, volume, boot)
}

/// Quotes a value for a POSIX shell: inside single quotes nothing is special except the
/// quote itself
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use cloudlease_core::time::UnixMillis;
    use cloudlease_provider::digitalocean::ResourceName;

    use super::{
        Boot, BootError, BootFile, BootPayload, InterruptionNotice, LeaseFile, MountPath,
        VolumeMount, check_fits, quote, render_user_data,
    };
    use crate::time::Timestamp;

    fn payload() -> BootPayload {
        BootPayload {
            files: vec![BootFile {
                path: "/etc/homebased/config.toml".to_owned(),
                mode: "0600".to_owned(),
                content: "join_token = \"it's $(secret)\"\n".to_owned(),
            }],
            run: vec![vec![
                "/bin/sh".to_owned(),
                "/usr/local/lib/homebased/boot.sh".to_owned(),
                "'; rm -rf / #".to_owned(),
            ]],
        }
    }

    fn lease() -> LeaseFile<'static> {
        LeaseFile {
            lease_id: "run_0123456789abcdef01234567-1",
            token: "cll_secret",
            api_base: "https://machines.example.test",
            hard_deadline_at: Timestamp(UnixMillis::new(1_795_572_000_000).unwrap()),
            heartbeat_ttl_secs: 600,
            interruption_notice: None,
        }
    }

    #[test]
    fn boot_data_that_quoting_inflates_past_the_user_data_limit_is_refused_up_front() {
        let mount = MountPath::parse("/mnt/data").unwrap();
        let api_base = "https://machines.example.test";
        assert_eq!(
            check_fits(&Boot::new(payload()).unwrap(), api_base, Some(&mount)).map(drop),
            Ok(())
        );

        // within the 32 KiB JSON limit, but every quote becomes four bytes once quoted
        let quotes = BootPayload {
            files: Vec::new(),
            run: vec![vec!["'".repeat(3_900); 8]],
        };
        let boot = Boot::new(quotes).unwrap();
        assert_eq!(
            check_fits(&boot, api_base, None).map(drop),
            Err(BootError::UserDataTooLarge)
        );
    }

    #[test]
    fn quoting_survives_a_real_shell() {
        let hostile = "it's \"$(touch /tmp/pwned)\" `id` \\ $HOME\nnext";
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", quote(hostile)))
            .output()
            .unwrap();

        assert_eq!(String::from_utf8(output.stdout).unwrap(), hostile);
    }

    #[test]
    fn user_data_writes_the_lease_first_then_the_volume_files_and_commands_in_order() {
        let name = ResourceName::parse("cl-run-1").unwrap();
        let mount = MountPath::parse("/mnt/data").unwrap();
        let script = render_user_data(
            &lease(),
            Some(VolumeMount {
                name: &name,
                mount: &mount,
            }),
            &Boot::new(payload()).unwrap(),
        )
        .unwrap();
        let script = script.as_str();

        let lease_at = script.find("'/etc/cloudlease/lease.json' 0600").unwrap();
        let volume_at = script.find("scsi-0DO_Volume_cl-run-1").unwrap();
        let file_at = script.find("'/etc/homebased/config.toml' '0600'").unwrap();
        let run_at = script
            .find("'/bin/sh' '/usr/local/lib/homebased/boot.sh'")
            .unwrap();
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(lease_at < volume_at && volume_at < file_at && file_at < run_at);
        // the hostile argument stays one quoted argument
        assert!(script.contains(r"''\''; rm -rf / #'"));

        // the lease and the file travel base64-encoded, so they arrive byte for byte
        let encoded = |needle: &str| {
            let line = script.lines().find(|line| line.contains(needle)).unwrap();
            let field = line.rsplit(' ').next().unwrap().trim_matches('\'');
            String::from_utf8(STANDARD.decode(field).unwrap()).unwrap()
        };
        let lease_json: serde_json::Value = serde_json::from_str(&encoded("lease.json")).unwrap();
        assert_eq!(lease_json["token"], "cll_secret");
        assert_eq!(lease_json["hard_deadline_at"], "2026-11-25T02:00:00Z");
        assert_eq!(lease_json["heartbeat_ttl_secs"], 600);
        // a provider that never takes a leased machine back names no notice at all
        assert!(lease_json.get("interruption_notice").is_none());
        assert_eq!(encoded("config.toml"), payload().files[0].content);
    }

    #[test]
    fn an_aws_lease_file_names_the_metadata_service_as_its_interruption_notice() {
        let lease = LeaseFile {
            interruption_notice: Some(InterruptionNotice::AwsImds),
            ..lease()
        };

        let json: serde_json::Value = serde_json::to_value(&lease).unwrap();

        assert_eq!(json["interruption_notice"], "aws_imds");
    }

    #[test]
    fn boot_data_outside_the_contract_is_refused() {
        let with = |change: fn(&mut BootPayload)| {
            let mut payload = payload();
            change(&mut payload);
            Boot::new(payload)
        };

        assert!(matches!(
            with(|p| p.files[0].path = "etc/relative".into()),
            Err(BootError::InvalidPath(_))
        ));
        assert!(with(|p| p.files[0].path = "/etc/../shadow".into()).is_err());
        assert!(with(|p| p.files[0].path = "/etc/x'y".into()).is_err());
        assert!(with(|p| p.files[0].path = "/etc/dir/".into()).is_err());
        assert!(matches!(
            with(|p| p.files[0].mode = "rw".into()),
            Err(BootError::InvalidMode(_))
        ));
        assert!(with(|p| p.files[0].mode = "0800".into()).is_err());
        assert!(matches!(
            with(|p| p.run.push(Vec::new())),
            Err(BootError::EmptyCommand(1))
        ));
        assert!(with(|p| p.run[0].push("a\0b".into())).is_err());
        assert!(matches!(
            with(|p| p.files = vec![p.files[0].clone(); 17]),
            Err(BootError::TooManyFiles(17))
        ));
        assert!(matches!(
            with(|p| p.run = vec![vec!["true".into()]; 9]),
            Err(BootError::TooManyRuns(9))
        ));
        assert!(matches!(
            with(|p| p.files[0].content = "x".repeat(33 * 1024)),
            Err(BootError::TooLarge(_))
        ));
        assert!(
            serde_json::from_value::<BootPayload>(serde_json::json!({
                "files": [], "run": [], "env": {}
            }))
            .is_err()
        );
    }
}
