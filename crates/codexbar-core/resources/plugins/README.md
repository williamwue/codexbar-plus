# Provider plugins

`provider-plugin-prelude.js` and the original bundled `*.js` files are copied verbatim
from upstream CodexBar (<https://github.com/steipete/CodexBar>, MIT — see
`LICENSE.upstream`).

The additional Windows-first conversions below port the corresponding upstream Swift
provider to the same public JavaScript contract:

- `aiand.js`, `chutes.js`, `deepinfra.js`, `elevenlabs.js`, `fireworks.js`
- `litellm.js`, `llmproxy.js`, `moonshot.js`, `neuralwatt.js`, `zenmux.js`

The Windows host (`src/plugin/`) implements the same native `host.*` bridge and
`defineProvider` global. When upstream publishes an equivalent JS conversion, replace
the Windows-first file with that upstream file and retain its contract fixtures.

Refresh with:

```
cp upstream-ref/Sources/CodexBarCore/Resources/Plugins/*.js crates/codexbar-core/resources/plugins/
rm crates/codexbar-core/resources/plugins/sucrase-3.35.1.min.js
```
