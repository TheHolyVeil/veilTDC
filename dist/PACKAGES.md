# Recommended packages (Arch / CachyOS)

Veil provides the session: compositor, bar, launcher, power menu, clipboard
and wallpaper. It does not ship its own apps; install whatever fits. The
ones below are what the author runs and binds in `config.lua`.

| Role | Package | Default bind |
|---|---|---|
| Terminal | `kitty` | Alt+Enter |
| Browser | `helium-browser` (AUR) | Alt+B |
| Files | `dolphin` | Alt+E |

Also useful on a bare session: `pipewire` + `wireplumber` (audio),
`polkit` (privilege prompts), and a font package such as `ttf-dejavu`.

Bind your own apps in `~/.config/veil/config.lua`; each launch bind also
gets a bar tile:

```lua
apps = {
  { "enter", "kitty" },
  { "b",     "helium-browser" },
  { "e",     "dolphin" },
}
```
