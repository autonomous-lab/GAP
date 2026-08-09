# CI configuration

`github-ci.yml` is the GitHub Actions workflow for this repo (fmt check,
clippy `-D warnings`, full test suite, examples build, cargo-audit CVE
scan).

It lives here instead of `.github/workflows/` because the deploy PAT
does not carry the `workflow` permission — GitHub rejects any push that
creates or updates workflow files without it.

**To activate:** grant the fine-grained PAT the **Workflows** repository
permission (GitHub → Settings → Developer settings → Fine-grained
tokens), then:

```bash
mkdir -p .github/workflows
git mv ci/github-ci.yml .github/workflows/ci.yml
git commit -m "Activate GitHub Actions CI"
git push
```
