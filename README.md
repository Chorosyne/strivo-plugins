# strivo-plugins

> [!IMPORTANT]
> **Superseded — this repository is archived and read-only.**
>
> Both plugins now live inside the main [StriVo](https://github.com/revoydotdev/strivo)
> repository as the in-tree workspace crate `crates/strivo-plugins`. StriVo no longer
> consumes this repo as a git dependency, and development continues there.
>
> Nothing here is maintained. Use `revoydotdev/strivo`.


First-party plugins for [StriVo](https://github.com/revoydotdev/strivo).

| Plugin    | Purpose                                                                 |
|-----------|-------------------------------------------------------------------------|
| `crunchr` | AI transcription + diarization + analysis (Voxtral via OpenRouter [default], Mistral direct, WhisperX/pyannote local, self-hosted Voxtral, Whisper CLI). Speaker Editor TUI modal renames per-recording labels, voice-sample auditioning, SRT/VTT export, mkvmerge soft-sub embed. |
| `archiver`| Recording organization + gallery rendering                              |

## Using

StriVo itself depends on this crate, so installing StriVo (e.g. via the AUR)
gives you both plugins out of the box. If you're building from source:

```bash
git clone https://github.com/revoydotdev/strivo-plugins.git ../strivo-plugins
git clone https://github.com/revoydotdev/strivo.git
cd strivo && cargo build --release
```

The two repos must live side-by-side (`../strivo-plugins` is a path dependency
of `strivo`).

## Writing your own plugin

Implement the `strivo::plugin::Plugin` trait in a new crate that depends on
`strivo` as a library:

```toml
[dependencies]
strivo = { git = "https://github.com/revoydotdev/strivo", tag = "v0.3.0" }
```

Register your plugin in a fork of StriVo's `main.rs`, or wait for dynamic
plugin loading (roadmap).

## License

[MIT](LICENSE)
