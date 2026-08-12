---
name: gloop-graph-authoring
description: Use gloop to find, create, edit, validate, and run agent graphs for beginners.
---

# gloop graph authoring

Use this skill when a person asks to make a gloop workflow, change its nodes,
choose a harness or model, or run an existing graph.

## Safe beginner flow

Run these commands from the project directory. Do not invent a filename before
checking the list.

~~~text
1. gloop graph list
2. Pick one name or file from the output.
3. For a visual editor, run:
   gloop graph edit NAME --gui
4. In the browser, click a step and choose what it should do. If unsure, keep
   the suggested AI/app and model, then press Save this helper.
5. Validate the saved path shown by gloop:
   gloop graph validate PATH
6. Run it only after validation succeeds:
   gloop run --graph PATH --repo .
~~~

NAME may be a built-in template such as direct or plan-implement-verify, a
saved project template, or a graph YAML path.

If the person wants a reusable project template, use the visual initializer:

~~~text
gloop graph init --gui
~~~

Or start from a known template:

~~~text
gloop graph init --name my-flow --from plan-implement-verify --gui
~~~

For a saved project template, update is the friendly alias:

~~~text
gloop graph update my-flow --gui
~~~

## When there is no browser

Use the selective terminal editor. It asks before changing nodes, edges, or
graph settings:

~~~text
gloop graph edit NAME
~~~

Choose Save and finish. If a menu or validation error is unclear, do not guess:
show the exact error and run gloop graph list again.

## English and Japanese

The browser editor starts in English by default. Use --lang ja to start in
Japanese, or press the language button in the editor:

~~~text
gloop graph edit NAME --gui --lang en
gloop graph edit NAME --gui --lang ja
~~~

The first screen is intentionally simple: name and wish, helpers, then save.
Choose "Ask AI", "Run a command", "Check the answer", or "Ask a person".
Node ids, edge kinds, fan-out, and other technical fields are under
"More settings" / "くわしい設定（上級者向け）". Do not open those settings
unless the person specifically asks for them.

## Important rules for an assistant

- Never run a graph before gloop graph validate PATH succeeds.
- Never overwrite a graph because a name looks similar. Use the exact path from
  gloop graph list.
- Built-in templates are not edited in place. The first save creates a copy
  under .gloop/graphs/.
- If the GUI reports that the file changed while it was open, reload it and
  save again; do not force the old copy over the new one.
- A Harness is the execution program or provider profile. A Model is the model
  identifier passed to that harness. If unsure, select an enabled profile and
  one of the offered model defaults.
- If a command fails, preserve the complete error, fix only the stated issue,
  and retry once. Do not silently fall back to another harness or model.
