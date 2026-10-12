# Scripted toolbar items (Lua)

Crusteo's bars are made of toolbar items, and you can write your own in Lua.
Each `*.lua` file in crusteo's data folder under `toolbars/` is a script:

| OS      | Folder                                              |
|---------|-----------------------------------------------------|
| Windows | `%APPDATA%\crusteo\toolbars\`                       |
| macOS   | `~/Library/Application Support/crusteo/toolbars/`   |
| Linux   | `$XDG_DATA_HOME/crusteo/toolbars/` (or `~/.local/share/...`) |

Scripts load when the window opens, and each reloads by itself when you
save it. Each runs on its own thread, so a slow `http.get` or a mistake in
one script never stalls the window or the other scripts; a script that
fails to load shows as a red `!` in the title bar, with the error in its
tooltip, and an `update` that fails shows the same on its item.

## Declaring an item

```lua
toolbar.item {
  id = "clock",            -- unique within the script
  title = "Clock",         -- what customizing calls it
  bar = "title",           -- "title", "strip" or "layout"; default "title"
  side = "right",          -- "left" or "right"; default "right"
  every = 30,              -- seconds between updates; default 60
  update = function(fleet)
    return { text = os.date("%H:%M"), icon = "clock", tooltip = "Local time" }
  end,
  click = function()
    return { toast = "It is " .. os.date("%c") }
  end,
}
```

`bar` and `side` only say where the item goes the first time; after that
it stays wherever you put it. Besides the three built-in bars, `bar` can
name one of your own, made with **+ New bar** while customizing:
`"bar-1"`, `"bar-2"` and so on, in the order you made them. A script that
names one you have not made makes it.

The **?** button in the title bar has **Open scripts folder**, which makes
the folder if needed and opens it.

A script may declare any number of items.

### `update(fleet)`

Called right away, then every `every` seconds, and again after each click.
It returns a string, or a table:

| Field     | Meaning |
|-----------|---------|
| `text`    | What the item says. |
| `icon`    | An icon name, e.g. `clock`, `activity`, `bot`, `server`, `sparkles`. Unknown names draw nothing. |
| `tooltip` | Shown on hover; defaults to the title. |
| `color`   | `muted` (default), `foreground`, `accent`, `success`, `warning`, `danger`, or `#rrggbb`. |

`fleet` is a summary of what crusteo sees, refreshed every two seconds:

```lua
{
  layout = "lanes",          -- the layout on screen
  hosts = 5, hosts_online = 4,
  agents = { needs_input = 1, failed = 0, working = 3, ready_to_review = 2, done = 40 },
}
```

### `click()`

Optional. It may return a table of things for crusteo to do:

| Field    | Effect |
|----------|--------|
| `url`    | Opens it in the browser. |
| `toast`  | Shows a short notification. |
| `layout` | Switches to a layout: `paseo`, `fleet`, `board`, `lanes`. |

## What scripts can use

Lua 5.4's standard library, plus:

- `http.get(url)` returns `body` or `nil, error`; it times out after 10
  seconds. It blocks only the script's own thread.
- `json.decode(text)` and `json.encode(value)`.
- `log(text)` writes to crusteo's standard error.

Scripts are your own code with the same reach as any program you run: the
`os` and `io` libraries are available. Only install scripts you trust.

## Examples

A CI badge:

```lua
toolbar.item {
  id = "ci", title = "CI", every = 120,
  update = function()
    local body, err = http.get("https://ci.example.com/api/main/status")
    if not body then return { text = "CI ?", color = "warning", tooltip = err } end
    local status = json.decode(body).state
    return {
      text = "CI " .. status,
      color = status == "passing" and "success" or "danger",
    }
  end,
  click = function() return { url = "https://ci.example.com/main" } end,
}
```

How busy the fleet is, in the bar over the layout:

```lua
toolbar.item {
  id = "busy", title = "Agents working", bar = "layout", every = 2,
  update = function(fleet)
    local working = fleet.agents.working or 0
    return {
      text = working .. " working on " .. fleet.hosts_online .. "/" .. fleet.hosts .. " hosts",
      icon = "activity",
      color = working > 0 and "accent" or "muted",
    }
  end,
}
```

Each Lua state has a 32 MiB allocation limit and each load, update or click has a one-million Lua-instruction budget. `http.get` times out after ten seconds and accepts at most 1 MiB of response text. A budget error is shown on the item; later updates can run again. `every` must be finite and at most one day, with a minimum of half a second. Event and click queues are bounded; clicks are ignored if the worker's queue is full. These limits cover Lua execution and the built-in HTTP helper; blocking native functions from Lua's standard libraries still run on the script thread.
