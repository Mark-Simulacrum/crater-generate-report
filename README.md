# Generate Crater Reports

## Usage

```
cargo run --release -- <crater experiment name> > report.md
```

The tool will print to stdout in a format that identifies the root crates responible as much as is
possible. It'll also query the crates.io API to attempt to get owners of the regressed crates so
that they can be pinged in relevant issues.

The general procedure is to group the top-level crates into groups based on the error and then file
issues for each (and/or document the groups on the PRs). The individual regressions (i.e., the
leaves) author's are not pinged in the output via wrapping in code (`@ghost`) to avoid an excessive
number of pings. The individual, non-root, regressions are hidden so that they don't overwhelm users
looking at the issues.

## Future work

If you're interested, file an issue to discuss implementation / interest!

 - Exposing error messages and attempting to identify commonalities among root regressions
 - Preparing issue templates
   - Text for what to do, when to do it
   - Possibly integration with triagebot to ping the issue as we get closer to release to make sure
     it's being looked at
