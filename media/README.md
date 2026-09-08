# gloop demo videos

Two silent 1920 × 1080, 30 fps videos rendered with Remotion 4.0.522.

- **Terminal — 31 seconds:** an actual gloop 0.8.1 process launched in a PTY,
  opening a saved graph, running three independent local commands, and combining
  their files in a fourth command. No model is called. The checks operate on a
  small synthetic dataset; their results are not gloop's own test-suite results.
- **Parallel — 21 seconds:** a conceptual animation of three independent tools
  finishing at different times before a dependent step combines their results.
  It illustrates orchestration, rather than recording live provider activity.

## Render the committed recording

Requires Node.js 22+, npm, and FFmpeg (for GIF previews).

```sh
cd media
npm ci
npm run prepare:terminal
npm run check
npm run stills
npm run render
```

Remotion downloads Chrome Headless Shell on the first render. MP4 files are
written to `assets/videos/` and published as GitHub release assets. Stills stay
in the ignored `media/out/` directory. The committed `.cast` file is the original
PTY output, including its timing. `@xterm/headless` interprets the terminal
control sequences; Remotion renders its resulting screen cells and the framing.
This preserves the real terminal session without recording the user's desktop.

The compositions are in `src/index.tsx`; the programmatic renderer uses
Remotion's [renderMedia API](https://www.remotion.dev/docs/renderer/render-media).

## Record another terminal take

```sh
cargo build -p gloop-cli --locked
python3 media/scripts/record-terminal.py
```

The recorder requires `/tmp/gloop-demo` to be absent and refuses to reuse an
existing workspace. Move a previous take somewhere else before recording again.
It creates only public sample data and a graph with three command nodes plus a
merge node. The fixture has deliberate 5/8/11-second holds and a 3-second merge
hold so the state transitions are visible. These delays are in the fixture;
gloop's product code is unchanged. The graph is limited to 30 seconds, three
parallel commands and zero model calls.

The shell starts without personal startup files and uses a plain `$` prompt.
The recorder rejects output containing the current username, home directory or
`/Users/` before saving. No desktop, personal project, credentials or microphone
are captured. Generic demo paths and gloop artifact paths remain visible.

## Preview GIFs and verification

```sh
cd media
python3 scripts/verify.py
sh scripts/previews.sh
```

Verification checks the original capture for private identifiers, validates that
all three recorded tasks overlapped and the merge started after all three
succeeded, and checks both MP4s for their expected duration, dimensions, frame
rate and absence of audio streams. `public/run-evidence.json` contains the
recorded run's lifecycle events and sample report.
