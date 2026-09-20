# jj-code-review

A TUI code-review tool for [Jujutsu](https://jj-vcs.github.io/jj/), callable by
agents. It opens a review over the diff `base..head`, collects comments,
suggested edits and deletions, and prints them to stdout on submit — plain
text, or JSON with `--json`. Nothing else goes to stdout, so the caller can
consume it directly.

## Install

```sh
cargo install jj-code-review
```

This installs the `jcr` binary. The `jj` CLI must be on `PATH`.

## Usage

```
jcr [REVSET] [--json] [--help] [--version]

ARGS:
    REVSET    Base revision of the review; the diff is shown as
              `REVSET..head`. Defaults to the revision before head.

OPTIONS:
    --json        Print the submitted review as JSON instead of plain text.
    -h, --help    Show this help.
    -V, --version Show the version.
```

`head` is `@`, unless `@` is both empty and undescribed, in which case head is
`@-` — so a bare `jcr` right after `jj commit` reviews the change you just
finished.

`jcr` needs an interactive terminal; running it without a TTY exits with code 2.

### Keys

```
Tab / Shift+Tab  cycle Diff · Files · Comments
j k ↑ ↓          move               ] [      next / previous hunk
n p              next / prev file   g G      first / last
v                visual selection   Enter    open file / goto comment
c                comment            e        suggested edit
x                mark for deletion  d        clear / delete annotation
?                help               s        submit
q / Esc / Ctrl-C quit without submitting
```

### Exit codes

| Code | Meaning                                                     |
| ---- | ----------------------------------------------------------- |
| 0    | review submitted (an empty review counts)                    |
| 1    | quit without submitting                                      |
| 2    | usage or runtime error (nothing is written to stdout)        |

## License

MIT
