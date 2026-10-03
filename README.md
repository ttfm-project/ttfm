# ttfm — Typed Tag File Manager

An innovative file manager based on typed tags (`type:label` format).

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/coponhub/TTFM/main/ttfm_cli.sh | sh
```

- Supported platforms: Linux x86_64, macOS Apple Silicon (11.0+)
- Other platforms: `cargo install --git https://github.com/coponhub/TTFM`
- Environment variables:
  - `TTFM_VERSION`: install a specific version, e.g. `0.1.0` (default: latest)
  - `TTFM_INSTALL_DIR`: install directory (default: `~/.local/bin`)

### Shell completion

- bash (4+) / fish: installed automatically
- zsh: add `source <(ttfm completion zsh)` to `~/.zshrc`
- bash without bash-completion: add `eval "$(ttfm completion bash)"` to `~/.bashrc`

### Uninstall

```sh
curl -fsSL https://raw.githubusercontent.com/coponhub/TTFM/main/ttfm_cli.sh | sh -s -- --uninstall
```

Add `--purge` to also remove user data (`TTFM_HOME`, default: `~/.ttfm`).

## License

Copyright (C) 2026 The TTFM Project Contributors
See the CONTRIBUTORS file at the top-level directory of this distribution for a list of copyright holders.

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
