* [ ] `rustup update stable`
* [ ] ensure `cargo +stable build --release --all-features` works
* [ ] bump version in Cargo.toml
  * [ ] did `tests::exercise_full_api` change? if so, it's a semver-breaking change.
* [ ] update changelog
* [ ] `cargo run --example update-readme`
* [ ] commit, tag, push commit, but do not push tag yet
* build binaries:
  * [ ] `script/release-tapioca`
  * [ ] `script/release-raven.ps1`
  * [ ] `script/release-ishtar`
  * [ ] `script/release-debian`
  * [ ] `script/release-talia`
* [ ] `cargo publish`
* [ ] push tag
* [ ] edit release to include changelog
