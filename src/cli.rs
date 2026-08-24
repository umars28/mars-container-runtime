use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "mars",
    version,
    about = "An OCI-compliant container runtime",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[arg(long, global = true, default_value = "/run/mars", value_name = "PATH")]
    pub root: PathBuf,

    #[arg(long, global = true, value_name = "PATH")]
    pub log: Option<PathBuf>,

    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Text, value_name = "FORMAT")]
    pub log_format: LogFormat,

    #[arg(long, global = true)]
    pub debug: bool,

    #[arg(long, global = true)]
    pub systemd_cgroup: bool,

    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "MODE",
        num_args = 0..=1,
        default_missing_value = "true",
    )]
    pub rootless: Option<Rootless>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rootless {
    True,
    False,
    Auto,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListFormat {
    Table,
    Json,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(about = "Create a container from a bundle without starting its process")]
    Create(CreateArgs),

    #[command(about = "Start the user process of a previously created container")]
    Start(StartArgs),

    #[command(about = "Create and immediately start a container")]
    Run(RunArgs),

    #[command(about = "Run a new process inside an already running container")]
    Exec(ExecArgs),

    #[command(about = "Send a signal to the container process")]
    Kill(KillArgs),

    #[command(about = "Delete a stopped container and release its resources")]
    Delete(DeleteArgs),

    #[command(about = "Print the OCI state of a container as JSON")]
    State(StateArgs),

    #[command(about = "List containers known to this runtime root")]
    List(ListArgs),

    #[command(about = "Show processes running inside a container")]
    Ps(PsArgs),

    #[command(about = "Freeze all processes in the container cgroup")]
    Pause(StateArgs),

    #[command(about = "Unfreeze a paused container")]
    Resume(StateArgs),

    #[command(about = "Stream container events and cgroup statistics")]
    Events(EventsArgs),

    #[command(about = "Change the resource limits of a running container")]
    Update(UpdateArgs),

    #[command(about = "Generate a default config.json in a bundle directory")]
    Spec(SpecArgs),

    #[command(about = "Print the features this runtime implements as JSON")]
    Features,
}

#[derive(Args, Debug)]
pub struct CreateArgs {
    #[arg(long, short = 'b', default_value = ".", value_name = "PATH")]
    pub bundle: PathBuf,

    #[arg(long, value_name = "PATH")]
    pub console_socket: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    pub pid_file: Option<PathBuf>,

    #[arg(long, default_value_t = 0, value_name = "N")]
    pub preserve_fds: i32,

    #[arg(long)]
    pub no_pivot: bool,

    #[arg(long)]
    pub no_new_keyring: bool,

    pub id: String,
}

#[derive(Args, Debug)]
pub struct StartArgs {
    pub id: String,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(long, short = 'b', default_value = ".", value_name = "PATH")]
    pub bundle: PathBuf,

    #[arg(long, value_name = "PATH")]
    pub console_socket: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    pub pid_file: Option<PathBuf>,

    #[arg(long, short = 'd')]
    pub detach: bool,

    #[arg(long, default_value_t = 0, value_name = "N")]
    pub preserve_fds: i32,

    #[arg(long)]
    pub no_pivot: bool,

    #[arg(long)]
    pub no_new_keyring: bool,

    #[arg(long)]
    pub keep: bool,

    pub id: String,
}

#[derive(Args, Debug)]
pub struct ExecArgs {
    #[arg(long, short = 'p', value_name = "PATH")]
    pub process: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    pub console_socket: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    pub cwd: Option<PathBuf>,

    #[arg(long, short = 'e', value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    #[arg(long, short = 't')]
    pub tty: bool,

    #[arg(long, short = 'u', value_name = "UID[:GID]")]
    pub user: Option<String>,

    #[arg(long, value_name = "GID", value_delimiter = ',')]
    pub additional_gids: Vec<u32>,

    #[arg(long, short = 'd')]
    pub detach: bool,

    #[arg(long, value_name = "PATH")]
    pub pid_file: Option<PathBuf>,

    #[arg(long, value_name = "LABEL")]
    pub process_label: Option<String>,

    #[arg(long, value_name = "PROFILE")]
    pub apparmor: Option<String>,

    #[arg(long)]
    pub no_new_privs: bool,

    #[arg(long, value_name = "CAP")]
    pub cap: Vec<String>,

    #[arg(long, default_value_t = 0, value_name = "N")]
    pub preserve_fds: i32,

    #[arg(long, value_name = "PATH")]
    pub cgroup: Option<String>,

    #[arg(long)]
    pub ignore_paused: bool,

    pub id: String,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub argv: Vec<String>,
}

#[derive(Args, Debug)]
pub struct KillArgs {
    #[arg(long, short = 'a')]
    pub all: bool,

    pub id: String,

    #[arg(default_value = "SIGTERM")]
    pub signal: String,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    #[arg(long, short = 'f')]
    pub force: bool,

    pub id: String,
}

#[derive(Args, Debug)]
pub struct StateArgs {
    pub id: String,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    #[arg(long, short = 'f', value_enum, default_value_t = ListFormat::Table, value_name = "FORMAT")]
    pub format: ListFormat,

    #[arg(long, short = 'q')]
    pub quiet: bool,
}

#[derive(Args, Debug)]
pub struct PsArgs {
    #[arg(long, short = 'f', default_value = "table", value_name = "FORMAT")]
    pub format: String,

    pub id: String,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub ps_options: Vec<String>,
}

#[derive(Args, Debug)]
pub struct EventsArgs {
    #[arg(long, default_value = "5s", value_name = "DURATION")]
    pub interval: String,

    #[arg(long)]
    pub stats: bool,

    pub id: String,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    #[arg(long, short = 'r', value_name = "PATH")]
    pub resources: Option<PathBuf>,

    #[arg(long, value_name = "BYTES")]
    pub memory: Option<i64>,

    #[arg(long, value_name = "BYTES")]
    pub memory_swap: Option<i64>,

    #[arg(long, value_name = "QUOTA")]
    pub cpu_quota: Option<i64>,

    #[arg(long, value_name = "PERIOD")]
    pub cpu_period: Option<u64>,

    #[arg(long, value_name = "SHARES")]
    pub cpu_share: Option<u64>,

    #[arg(long, value_name = "LIST")]
    pub cpuset_cpus: Option<String>,

    #[arg(long, value_name = "LIST")]
    pub cpuset_mems: Option<String>,

    #[arg(long, value_name = "N")]
    pub pids_limit: Option<i64>,

    pub id: String,
}

#[derive(Args, Debug)]
pub struct SpecArgs {
    #[arg(long, short = 'b', default_value = ".", value_name = "PATH")]
    pub bundle: PathBuf,
}
