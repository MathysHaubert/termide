//! Shell completion scripts: `--completions <shell>` prints one,
//! `--install-completions [<shell>]` writes it where that shell looks.
//!
//! The scripts are hand-written and live in `completions/` so packagers can
//! install them without running the binary; they are embedded here so a
//! machine that has only the binary can still get them. `main.rs` carries a
//! test that fails when a clap option is missing from any of them.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

pub const SHELLS: [&str; 3] = ["bash", "zsh", "fish"];

/// The completion script shipped for `shell`.
pub fn script(shell: &str) -> &'static str {
    match shell {
        "bash" => include_str!("../completions/termide.bash"),
        "zsh" => include_str!("../completions/_termide"),
        "fish" => include_str!("../completions/termide.fish"),
        other => unreachable!("clap rejects unknown shell {other:?}"),
    }
}

/// The environment `install` reads, gathered up front so the path logic can
/// be tested without touching the real home directory.
pub struct Env {
    pub home: PathBuf,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub bash_completion_user_dir: Option<PathBuf>,
    pub zdotdir: Option<PathBuf>,
    pub shell: Option<String>,
}

impl Env {
    pub fn from_process() -> Result<Self> {
        let path = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Ok(Self {
            home: dirs::home_dir().ok_or_else(|| anyhow!("cannot determine the home directory"))?,
            xdg_config_home: path("XDG_CONFIG_HOME"),
            xdg_data_home: path("XDG_DATA_HOME"),
            bash_completion_user_dir: path("BASH_COMPLETION_USER_DIR"),
            zdotdir: path("ZDOTDIR"),
            shell: std::env::var("SHELL").ok(),
        })
    }

    /// The shell named by `$SHELL`, when it is one we have a script for.
    pub fn login_shell(&self) -> Option<&'static str> {
        let name = Path::new(self.shell.as_deref()?).file_name()?.to_str()?;
        SHELLS.iter().copied().find(|shell| *shell == name)
    }

    /// Where `shell` will find a per-user completion file for termide.
    ///
    /// bash and fish have directories their completion machinery scans on
    /// its own; zsh has no such default, so `~/.zfunc` is used and the user
    /// is told to put it on `$fpath`.
    pub fn destination(&self, shell: &str) -> PathBuf {
        match shell {
            "bash" => self
                .bash_completion_user_dir
                .clone()
                .unwrap_or_else(|| self.xdg_data_home().join("bash-completion"))
                .join("completions")
                .join("termide"),
            "zsh" => self.home.join(".zfunc").join("_termide"),
            "fish" => self
                .xdg_config_home
                .clone()
                .unwrap_or_else(|| self.home.join(".config"))
                .join("fish")
                .join("completions")
                .join("termide.fish"),
            other => unreachable!("clap rejects unknown shell {other:?}"),
        }
    }

    fn xdg_data_home(&self) -> PathBuf {
        self.xdg_data_home
            .clone()
            .unwrap_or_else(|| self.home.join(".local").join("share"))
    }

    /// The zsh startup files that may put `~/.zfunc` on `$fpath`: `.zshrc`
    /// (before `compinit`) or `.zshenv`, which is read before `.zshrc`.
    pub fn zsh_rc_files(&self) -> [PathBuf; 2] {
        let dir = self.zdotdir.clone().unwrap_or_else(|| self.home.clone());
        [dir.join(".zshrc"), dir.join(".zshenv")]
    }
}

/// Whether an rc file already adds `~/.zfunc` to `$fpath`. Deliberately loose:
/// any uncommented line that assigns `fpath` and mentions `.zfunc` counts,
/// however the path is spelled.
pub fn zshrc_mentions_zfunc(rc: &str) -> bool {
    uncommented_lines(rc).any(|line| line.contains("fpath") && line.contains(".zfunc"))
}

/// Whether an rc file turns the completion system on. Frameworks (Oh My Zsh,
/// prezto, zinit, ...) call `compinit` from their own files, so a plain
/// `source` of one of them counts too; a false negative here only costs the
/// user a hint they did not need.
pub fn zshrc_enables_completion(rc: &str) -> bool {
    const FRAMEWORKS: [&str; 6] = ["oh-my-zsh", "prezto", "zinit", "zim", "antidote", "zplug"];
    uncommented_lines(rc)
        .any(|line| line.contains("compinit") || FRAMEWORKS.iter().any(|f| line.contains(f)))
}

fn uncommented_lines(rc: &str) -> impl Iterator<Item = &str> {
    rc.lines()
        .map(str::trim_start)
        .filter(|line| !line.starts_with('#'))
}

/// Write the completion script for `shell` (or for `$SHELL` when `None`) to
/// its per-user location and return the report to print.
///
/// Never edits an rc file: for bash and fish none needs editing, and for zsh
/// the user is told which line to add and where.
pub fn install(shell: Option<&str>, env: &Env) -> Result<String> {
    let shell = match shell {
        Some(shell) => shell,
        None => env.login_shell().ok_or_else(|| {
            anyhow!(
                "cannot tell the shell from $SHELL ({}); pass it explicitly: \
                 --install-completions <bash|zsh|fish>",
                env.shell.as_deref().unwrap_or("unset")
            )
        })?,
    };

    let dest = env.destination(shell);
    let script = script(shell);
    let mut report = String::new();

    if std::fs::read_to_string(&dest).ok().as_deref() == Some(script) {
        report.push_str(&format!("{} is already up to date.\n", dest.display()));
    } else {
        let dir = dest
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", dest.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&dest, script).with_context(|| format!("writing {}", dest.display()))?;
        report.push_str(&format!("Wrote {}.\n", dest.display()));
    }

    match shell {
        "bash" => report.push_str(
            "bash-completion loads it in new shells. If `termide --<Tab>` still \
             completes nothing, bash-completion is not installed; put this in \
             ~/.bashrc instead:\n  eval \"$(termide --completions bash)\"\n",
        ),
        "fish" => report.push_str("fish picks it up in new sessions.\n"),
        "zsh" => {
            let [zshrc, zshenv] = env.zsh_rc_files();
            let rc_texts: Vec<(&Path, String)> = [&zshrc, &zshenv]
                .into_iter()
                .filter_map(|rc| Some((rc.as_path(), std::fs::read_to_string(rc).ok()?)))
                .collect();
            let on_fpath = rc_texts
                .iter()
                .find(|(_, text)| zshrc_mentions_zfunc(text))
                .map(|(rc, _)| *rc);
            match on_fpath {
                Some(rc) => report.push_str(&format!(
                    "{} already puts ~/.zfunc on $fpath. Run `rm -f ~/.zcompdump*` \
                     and start a new shell.\n",
                    rc.display()
                )),
                None => report.push_str(&format!(
                    "Add this line to {} before `compinit`, or to {}, then start a \
                     new shell:\n  fpath=(~/.zfunc $fpath)\n",
                    zshrc.display(),
                    zshenv.display()
                )),
            }
            // A zsh that never runs compinit has completion switched off for
            // every command, so the file above changes nothing until it does.
            if !rc_texts
                .iter()
                .any(|(_, text)| zshrc_enables_completion(text))
            {
                report.push_str(&format!(
                    "Nothing in {} runs compinit, which is what turns completion on \
                     in zsh; add this line to it:\n  autoload -Uz compinit && compinit\n",
                    zshrc.display()
                ));
            }
        }
        _ => bail!("no completion script for {shell}"),
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &str) -> Env {
        Env {
            home: PathBuf::from(home),
            xdg_config_home: None,
            xdg_data_home: None,
            bash_completion_user_dir: None,
            zdotdir: None,
            shell: Some("/bin/zsh".into()),
        }
    }

    #[test]
    fn destinations_follow_each_shells_convention() {
        let e = env("/home/u");
        assert_eq!(
            e.destination("bash"),
            PathBuf::from("/home/u/.local/share/bash-completion/completions/termide")
        );
        assert_eq!(
            e.destination("zsh"),
            PathBuf::from("/home/u/.zfunc/_termide")
        );
        assert_eq!(
            e.destination("fish"),
            PathBuf::from("/home/u/.config/fish/completions/termide.fish")
        );
    }

    #[test]
    fn xdg_and_shell_specific_overrides_win() {
        let mut e = env("/home/u");
        e.xdg_data_home = Some("/data".into());
        e.xdg_config_home = Some("/cfg".into());
        e.zdotdir = Some("/zdot".into());
        assert_eq!(
            e.destination("bash"),
            PathBuf::from("/data/bash-completion/completions/termide")
        );
        assert_eq!(
            e.destination("fish"),
            PathBuf::from("/cfg/fish/completions/termide.fish")
        );
        assert_eq!(
            e.zsh_rc_files(),
            [
                PathBuf::from("/zdot/.zshrc"),
                PathBuf::from("/zdot/.zshenv")
            ]
        );
        e.bash_completion_user_dir = Some("/bc".into());
        assert_eq!(
            e.destination("bash"),
            PathBuf::from("/bc/completions/termide")
        );
    }

    #[test]
    fn login_shell_comes_from_the_basename_of_shell() {
        let mut e = env("/h");
        assert_eq!(e.login_shell(), Some("zsh"));
        e.shell = Some("/opt/homebrew/bin/fish".into());
        assert_eq!(e.login_shell(), Some("fish"));
        e.shell = Some("/bin/tcsh".into());
        assert_eq!(e.login_shell(), None);
        e.shell = None;
        assert_eq!(e.login_shell(), None);
    }

    #[test]
    fn zshrc_check_ignores_comments_and_needs_both_words() {
        assert!(zshrc_mentions_zfunc("fpath=(~/.zfunc $fpath)\n"));
        assert!(zshrc_mentions_zfunc("  fpath+=(\"$HOME/.zfunc\")"));
        assert!(!zshrc_mentions_zfunc("# fpath=(~/.zfunc $fpath)"));
        assert!(!zshrc_mentions_zfunc("fpath=(~/.zsh/completions $fpath)"));
        assert!(!zshrc_mentions_zfunc("source ~/.zfunc/helpers"));
    }

    #[test]
    fn completion_is_enabled_by_compinit_or_a_framework() {
        assert!(zshrc_enables_completion(
            "autoload -Uz compinit && compinit\n"
        ));
        assert!(zshrc_enables_completion("source $ZSH/oh-my-zsh.sh"));
        assert!(zshrc_enables_completion(
            "zinit light zsh-users/zsh-completions"
        ));
        assert!(!zshrc_enables_completion(
            "# autoload -Uz compinit; compinit"
        ));
        assert!(!zshrc_enables_completion(
            "export PATH=~/bin:$PATH\nalias ll='ls -l'\n"
        ));
    }

    #[test]
    fn install_writes_the_script_and_reports_idempotently() {
        let home = tempfile::tempdir().unwrap();
        let e = env(home.path().to_str().unwrap());

        let first = install(None, &e).unwrap();
        let dest = e.destination("zsh");
        assert!(
            first.starts_with(&format!("Wrote {}.", dest.display())),
            "{first}"
        );
        assert!(first.contains("fpath=(~/.zfunc $fpath)"), "{first}");
        assert!(
            first.contains("autoload -Uz compinit && compinit"),
            "{first}"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), script("zsh"));

        let second = install(Some("zsh"), &e).unwrap();
        assert!(second.contains("already up to date"), "{second}");

        let [zshrc, zshenv] = e.zsh_rc_files();
        std::fs::write(&zshenv, "fpath=(~/.zfunc $fpath)\n").unwrap();
        let third = install(Some("zsh"), &e).unwrap();
        assert!(
            third.contains(&format!(
                "{} already puts ~/.zfunc on $fpath",
                zshenv.display()
            )),
            "{third}"
        );
        assert!(third.contains("runs compinit"), "{third}");
        assert!(!zshrc.exists(), "install must not create rc files");

        // fpath in .zshenv and compinit in .zshrc: no hint is left to give.
        std::fs::write(&zshrc, "autoload -Uz compinit && compinit\n").unwrap();
        let fourth = install(Some("zsh"), &e).unwrap();
        assert!(!fourth.contains("runs compinit"), "{fourth}");
        assert_eq!(fourth.lines().count(), 2, "{fourth}");
    }

    #[test]
    fn install_without_a_recognised_shell_asks_for_one() {
        let mut e = env("/nonexistent");
        e.shell = Some("/bin/tcsh".into());
        let err = install(None, &e).unwrap_err().to_string();
        assert!(
            err.contains("/bin/tcsh") && err.contains("--install-completions <bash|zsh|fish>"),
            "{err}"
        );
    }
}
