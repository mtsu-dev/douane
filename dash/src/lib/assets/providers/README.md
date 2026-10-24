# Provider marks

Static SVG brand marks bundled with the dashboard. No package or runtime CDN is needed. These marks identify the connected provider; they are not Douane's own brand and do not imply endorsement.

| Provider | File | Source | Rights and palette |
| --- | --- | --- | --- |
| Claude | `claude.svg` | [LobeHub 1.95.1, Claude color](https://unpkg.com/@lobehub/icons-static-svg@1.95.1/icons/claude-color.svg) | MIT library asset; original Claude orange. |
| Codex | `codex.svg` | [LobeHub 1.95.1, OpenAI](https://unpkg.com/@lobehub/icons-static-svg@1.95.1/icons/openai.svg), identified in [OpenAI brand guidelines](https://openai.com/brand/) | MIT library asset; white OpenAI blossom for the account provider. |
| OpenAI | `openai.svg` | [LobeHub 1.95.1, OpenAI](https://unpkg.com/@lobehub/icons-static-svg@1.95.1/icons/openai.svg) | MIT library asset; white monochrome variant. |
| OpenCode Go | `opencode-go.svg` | [Official OpenCode dark logo](https://github.com/anomalyco/opencode/blob/dev/packages/console/app/src/asset/brand/opencode-logo-dark.svg), published in [brand assets](https://opencode.ai/brand) | [MIT](https://github.com/anomalyco/opencode/blob/dev/LICENSE); official OpenCode symbol, original colors. |
| Grok / xAI | `xai.svg` | [LobeHub 1.95.1, Grok](https://unpkg.com/@lobehub/icons-static-svg@1.95.1/icons/grok.svg) | MIT library asset; white Grok symbol. |
| Antigravity | `antigravity.svg` | [Official Google white icon](https://antigravity.google/assets/image/brand/antigravity-icon__white.svg), listed in [press assets](https://antigravity.google/press) | Google brand asset, not MIT; retain Google's [brand guidelines](https://about.google/brand-resource-center/guidance/). Unmodified white icon, distinct from Gemini. |
| Gemini | `gemini.svg` | [LobeHub 1.95.1, Gemini color](https://unpkg.com/@lobehub/icons-static-svg@1.95.1/icons/gemini-color.svg) | MIT library asset; original multicolor star. |
| Kimi | `kimi.svg` | [LobeHub 1.95.1, Kimi color](https://unpkg.com/@lobehub/icons-static-svg@1.95.1/icons/kimi-color.svg) | MIT library asset; original white K and blue dot, transparent background. |
| GitHub Copilot | `copilot.svg` | [LobeHub 1.95.1, GithubCopilot](https://unpkg.com/@lobehub/icons-static-svg@1.95.1/icons/githubcopilot.svg) | MIT library asset; white monochrome variant. |
| Muse Code / Meta | `meta.svg` | [LobeHub 1.95.1, Meta color](https://unpkg.com/@lobehub/icons-static-svg@1.95.1/icons/meta-color.svg) | MIT library asset; original blue Meta symbol. |
| Qwen | `qwen.svg` | [LobeHub 1.95.1, Qwen color](https://unpkg.com/@lobehub/icons-static-svg@1.95.1/icons/qwen-color.svg) | MIT library asset; original violet gradient. |

LobeHub's package metadata declares MIT and points to [the library license](https://github.com/lobehub/lobe-icons/blob/master/LICENSE). Logo trademarks remain with their respective owners. Google's press asset is recorded separately rather than represented as an open-source grant.

OpenAI-compatible is a protocol category, not a provider brand. Use a neutral connection symbol for `openai_compatible`; no new or invented brand mark is included. `openai.svg` is available when explicitly identifying OpenAI itself.

## Rendering

Use the files as transparent images, normally at 24 px. The white monochrome variants remain visible when loaded through an image element on dark navy surfaces; they do not rely on inherited currentColor. Preserve the color marks rather than applying a hue or inversion filter. Codex uses the OpenAI provider mark. Antigravity retains its official padding; a 24-28 px image produces an approximately 18-21 px visible symbol.

Paths, gradients and view boxes were preserved. The library marks only lose presentation-only inline layout styles and change 1em dimensions to 24 px. Monochrome currentColor fills are fixed to white for external image rendering. OpenCode's dark logo and Antigravity's white icon preserve their official SVG shapes and colors. No image tracing, generated artwork, scripts, external references or embedded raster images are included.

## MIT notices for the library and OpenCode assets

Copyright (c) 2023 LobeHub

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
