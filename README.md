# strivo-plugins

First-party plugins for [StriVo](https://github.com/revelri/strivo).

| Plugin    | Purpose                                                                 |
|-----------|-------------------------------------------------------------------------|
| `crunchr` | AI transcription + diarization + analysis (Voxtral via OpenRouter [default], Mistral direct, WhisperX/pyannote local, self-hosted Voxtral, Whisper CLI). Speaker Editor TUI modal renames per-recording labels, voice-sample auditioning, SRT/VTT export, mkvmerge soft-sub embed. |
| `archiver`| Recording organization + gallery rendering                              |

## Using

StriVo itself depends on this crate, so installing StriVo (e.g. via the AUR)
gives you both plugins out of the box. If you're building from source:

```bash
git clone https://github.com/revelri/strivo-plugins.git ../strivo-plugins
git clone https://github.com/revelri/strivo.git
cd strivo && cargo build --release
```

The two repos must live side-by-side (`../strivo-plugins` is a path dependency
of `strivo`).

## Writing your own plugin

Implement the `strivo::plugin::Plugin` trait in a new crate that depends on
`strivo` as a library:

```toml
[dependencies]
strivo = { git = "https://github.com/revelri/strivo", tag = "v0.3.0" }
```

Register your plugin in a fork of StriVo's `main.rs`, or wait for dynamic
plugin loading (roadmap).

## License

[MIT](LICENSE)
