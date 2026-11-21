use clap::{Arg, ArgAction, CommandFactory, FromArgMatches, Parser, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = tr!("SD card imaging and rescue tool with timeout detection and reconnect recovery", "タイムアウト検知・再接続復旧に対応したSDカード救出ツール")
)]
pub(crate) struct Args {
    /// Source drive letter (e.g. E: or E), physical disk number (e.g. 2), or auto
    #[arg(short, long, default_value = "auto")]
    pub(crate) drive: String,

    /// Read order: random expands fast regions, falling back to a random location
    #[arg(long, alias = "directon", value_enum, default_value_t = Direction::Random)]
    pub(crate) direction: Direction,

    /// Output image path (detailed events are appended to OUTPUT.log)
    #[arg(value_name = "OUTPUT", default_value = "sd_backup.img")]
    pub(crate) out: String,

    /// Initial/minimum read timeout in seconds; scales with successful transfer time and size
    #[arg(short, long, default_value_t = 2.0)]
    pub(crate) timeout: f64,

    /// Health-check observation interval and slow-operation warning threshold, in seconds
    #[arg(long, default_value_t = 10.0)]
    pub(crate) operation_timeout: f64,

    /// Maximum outstanding scan reads; one separate slot is reserved for reference checks
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..=64))]
    pub(crate) max_parallel_reads: u32,

    /// Upper bound for adaptive reads (e.g. 4MiB, 1GiB); buffers grow only as needed
    #[arg(long, default_value = "1GiB", value_parser = parse_read_size)]
    pub(crate) max_read_size: usize,

    /// Rescan legacy bad (-) sectors from a previous session
    #[arg(short, long, default_value_t = false)]
    pub(crate) retry_bad: bool,
}

#[derive(ValueEnum, Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Direction {
    Forward,
    Reverse,
    Random,
}

impl Direction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Forward => tr!("forward", "順方向"),
            Self::Reverse => tr!("reverse", "逆方向"),
            Self::Random => tr!(
                "random (fast neighbors first)",
                "ランダム（正常領域の前後を優先）"
            ),
        }
    }
}

impl Args {
    pub(crate) fn parse_localized() -> Self {
        let mut command = Self::command();
        if crate::i18n::japanese() {
            for (id, text) in [
                (
                    "drive",
                    "対象のドライブレター（E:）、物理ディスク番号（2）、またはauto",
                ),
                (
                    "direction",
                    "読取順序。randomでは速く読めた領域の未探索側を優先し、候補がなければランダム",
                ),
                (
                    "out",
                    "出力イメージのパス。詳細ログは同じパスに.logを付けて追記します",
                ),
                (
                    "timeout",
                    "読取タイムアウトの初期値・下限（秒）。成功時の読取時間とサイズから自動調整",
                ),
                (
                    "operation_timeout",
                    "正常領域の観測時間・操作遅延を通知するまでの秒数",
                ),
                (
                    "max_parallel_reads",
                    "走査の未完了要求数の上限（正常確認は別枠1件、通常走査は1件ずつ）",
                ),
                (
                    "max_read_size",
                    "自動調整する読取サイズの上限（例: 4MiB、1GiB）。必要時だけメモリーを確保",
                ),
                ("retry_bad", "以前のマップで旧NG（-）のセクタを再走査する"),
            ] {
                command = command.mut_arg(id, |argument| {
                    argument.help(text).help_heading(if id == "out" {
                        "引数"
                    } else {
                        "オプション"
                    })
                });
            }
            command = command
                .disable_help_flag(true)
                .disable_version_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::Help)
                        .help("ヘルプを表示")
                        .help_heading("オプション"),
                )
                .arg(
                    Arg::new("version")
                        .long("version")
                        .short('V')
                        .action(ArgAction::Version)
                        .help("バージョンを表示")
                        .help_heading("オプション"),
                )
                .help_template("{about-with-newline}\n使用方法: {usage}\n\n{all-args}");
        }
        let matches = command.get_matches();
        Self::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
    }
}

fn parse_read_size(value: &str) -> Result<usize, String> {
    let value = value.trim().to_ascii_lowercase();
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let number = value[..split]
        .parse::<u64>()
        .map_err(|_| tr!("Invalid read size", "読取サイズが不正です").to_owned())?;
    let multiplier = match value[split..].trim() {
        "" | "b" => 1,
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        "kb" => 1000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        _ => {
            return Err(tr!(
                "Use bytes, KiB, MiB, GiB, KB, MB, or GB",
                "bytes、KiB、MiB、GiB、KB、MB、GBで指定してください"
            )
            .to_owned())
        }
    };
    let bytes = number
        .checked_mul(multiplier)
        .filter(|&bytes| (65536..=u32::MAX as u64).contains(&bytes))
        .ok_or_else(|| {
            tr!(
                "Read size must be between 64 KiB and 4294967295 bytes",
                "読取サイズは64 KiB以上4294967295バイト以下を指定してください"
            )
            .to_owned()
        })?;
    usize::try_from(bytes)
        .map_err(|_| tr!("Read size is too large", "読取サイズが大きすぎます").to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parallel_limit_defaults_to_sixteen_and_accepts_up_to_sixty_four() {
        assert_eq!(
            Args::try_parse_from(["sd2img"]).unwrap().max_parallel_reads,
            16
        );
        assert_eq!(
            Args::try_parse_from(["sd2img", "--max-parallel-reads", "16"])
                .unwrap()
                .max_parallel_reads,
            16
        );
        assert_eq!(
            Args::try_parse_from(["sd2img", "--max-parallel-reads", "64"])
                .unwrap()
                .max_parallel_reads,
            64
        );
        assert!(Args::try_parse_from(["sd2img", "--max-parallel-reads", "0"]).is_err());
        assert!(Args::try_parse_from(["sd2img", "--max-parallel-reads", "65"]).is_err());
    }

    #[test]
    fn adaptive_cap_accepts_binary_and_decimal_units_and_defaults_to_one_gib() {
        assert_eq!(
            Args::try_parse_from(["sd2img"]).unwrap().max_read_size,
            1 << 30
        );
        assert_eq!(parse_read_size("4MiB").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_read_size("4MB").unwrap(), 4_000_000);
        assert_eq!(parse_read_size("65536").unwrap(), 65536);
        assert!(parse_read_size("32KiB").is_err());
        assert!(parse_read_size("4GiB").is_err());
        assert!(parse_read_size("NaN").is_err());
    }
    #[test]
    fn cli_defaults_and_positional_output() {
        let args = Args::try_parse_from(["sd2img", "card image.img"]).unwrap();
        assert_eq!(args.out, "card image.img");
        assert_eq!(args.drive, "auto");
        assert_eq!(args.direction, Direction::Random);
        let args = Args::try_parse_from(["sd2img", "out.img", "-d", "2", "--direction", "reverse"])
            .unwrap();
        assert_eq!(args.direction, Direction::Reverse);
        assert!(Args::try_parse_from(["sd2img", "-o", "out.img"]).is_err());
        assert!(Args::try_parse_from(["sd2img", "--visual"]).is_err());
    }
}
