# inphase.sam-bennett.dev

The project's website: one static page, no build step. `.github/workflows/pages.yml`
publishes this directory to GitHub Pages when it changes on `main`; `CNAME` sets the
custom domain.

- The download button links to the latest release's `InPhaseSetup.exe`; the page
  names the version and size once the GitHub API answers.
- The donation button points at Buy Me a Coffee (`#coffee` in `index.html`).
- `assets/og.png` is the link preview (1200x630), rendered from the iPhone
  screenshot in `assets/iphone-library.*`.

## Hosting setup (one-time)

- Repository Settings -> Pages -> Source: GitHub Actions.
- Cloudflare DNS for sam-bennett.dev: `CNAME inphase -> sambennettdev.github.io`,
  DNS only (grey cloud), so GitHub can issue the HTTPS certificate.
- After the first deploy, tick Enforce HTTPS under Settings -> Pages.
