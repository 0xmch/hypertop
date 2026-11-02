# hypertop

`top`, for every device you own: peek at many at once and act on any number of them,
from one terminal.

```
 hypertop  14 devices  9 up  3 off
    NAME             KIND                     CPU   MEM  DISK   GPU  TEMP   POWER     UP NOTE
─ computers ──────────────────────────────────────────────────────────────────────── 3/4 up
  ○ handheld         Linux
  ● laptop           macOS              ▃▃▃   35%   59%   55%         48°             2d macOS
  ● nas              Linux               ▁▁    3%   66%   81%         41°            40d Debian
  ● workstation      Windows             ▂▂   19%   40%   14%   16%                  24h Windows
─ power ──────────────────────────────────────────────────────────────────────────── 2/2 up
  ● desk plug        plug                                                     12W
  ● UPS              ups                                                     180W        on mains
```

## Install

The [releases page](https://github.com/0xmch/hypertop/releases) has builds for macOS (Apple
Silicon) and Linux (x86-64), each with the bundled sources next to the binary. Those sources
need Python 3. It doesn't run on Windows, though it can watch Windows machines over ssh.

Unpack one, then link the sources you want:

```
mkdir -p ~/.config/hypertop/sources
ln -s "$PWD"/sources/ssh ~/.config/hypertop/sources/ssh   # and any other sources you want
./hypertop
```

With [mise](https://mise.jdx.dev), `mise use -g github:0xmch/hypertop` installs it. mise hides
a release for its first 24 hours unless told otherwise.

To build it yourself you need Rust:

```
git clone https://github.com/0xmch/hypertop
cd hypertop
cargo build --release
```

Every source lives in `~/.config/hypertop/sources`, whichever way you installed it.

## The idea

hypertop knows nothing about any kind of device. A device is anything a program can
describe. Every executable in `~/.config/hypertop/sources/` is run, and prints JSON
lines: a device's numbers, what can be peeked at, and which commands are its actions.
Adding a kind of device means adding a short script, whatever the language.

**Peeking costs nothing until you look.** The table shows what sources report at their
own pace. A peek tab runs its command only while it is on screen, and only once you
have stopped on that device, so moving through the list asks nothing of what you pass.

**Acting scales.** Tag devices with space (`t` tags everything shown, so filter and then
`t` tags a whole class), then press Enter for actions or `!` for a command. It goes to
everything tagged, shown or not, as the title counts them. Each device runs it at once and
reports back on its own, so one failure doesn't hide among the rest.

**Actions ask when they should.** A source marks what needs confirming. One device takes
`y`; several take a capital `Y`, because a stray key must not switch off a room. What runs
is what you confirmed, even if the list changes while you're being asked.

## Keys

The footer shows the common ones, `?` shows them all, `q` or Ctrl+C quits.

`hypertop --list` prints the same picture without the interface.

## Writing a source

A source prints one JSON object per line. If it exits, it runs again `every` seconds
later (`{"every": 30}`, default 30), and devices it stopped mentioning go away. Or it
can keep running and keep printing. Its stdin is a pipe that closes when hypertop ends,
so a long-running source can notice.

```python
#!/usr/bin/env python3
import json
print(json.dumps({"every": 60}))
print(json.dumps({
    "id": "printer",                 # required, unique within this source
    "name": "Printer",
    "group": "office",               # the section it appears under
    "kind": "printer",
    "state": "up",                   # up, idle, down, or anything else for unknown
    "note": "paper low",             # free text at the end of the row
    "alert": None,                   # text here marks it as needing attention
    "metrics": {"watts": 12},        # numbers; see below
    "info": {"address": "192.0.2.9"},
    "peeks": [{"label": "queue", "run": ["lpq"], "every": 5}],
    "actions": [
        {"id": "wake", "label": "Wake", "run": ["wakeonlan", "aa:bb:cc:dd:ee:ff"]},
        {"id": "off", "label": "Turn off", "run": ["plug", "off"], "confirm": True},
        {"id": "run", "label": "Run a command", "run": ["ssh", "printer", "{input}"], "input": "Command"},
        {"id": "shell", "label": "Open a shell", "run": ["ssh", "printer"], "tty": True},
    ],
}))
```

A later line with the same `id` only changes the fields it mentions, so a long-running
source can describe a device once and then send only its numbers.

- **Metrics** with a column of their own: `cpu`, `mem`, `disk`, `gpu` (percent), `temp`
  (°C), `watts`, `uptime` (seconds). Any other name shows in the overview. A column
  appears when some device has that number.
- **Attention**: `disk` from 90, `mem` from 95, and `temp` or `gputemp` from 85 flag a
  device by themselves, while it is up. So does a source that fails or prints something
  that isn't JSON, in a row of its own that keeps its devices listed, so a broken source
  never looks like its devices being fine.
- **Actions** match across devices by `id`: tag ten machines, and "Run a command" runs on
  all of them. `tty` actions take over the terminal and so work on one device at a time.
  `{input}` is replaced by what you type.
- **Output** from devices has its terminal control codes removed before it is shown.

To remove a source, delete it or make it non-executable.

## The bundled sources

Link the ones you want into `~/.config/hypertop/sources/`.

- **`sources/ssh`**: every host in `~/.ssh/config`, minus the aliases listed in
  `~/.config/hypertop/ssh-skip`. Nothing is installed on the hosts. Each gets one small
  sampler, over one connection, for as long as hypertop runs, and the sampler ends with
  the connection. Linux, macOS and Windows; on Windows it runs below normal priority, so
  it stays out of a game's way. Temperature comes from the sensors on Linux and from the
  die sensors on a Mac, with no sudo, if Homebrew's Python is there.
  Everything goes through `ssh`, so `ProxyJump` and the like work, and this machine needs
  its own ssh server if it's listed. While a machine is up it is asked for its MAC address,
  which is remembered so that it can be woken with a magic packet once it is off. A machine
  shown as off is one ssh can't reach: it may well be on, with ssh closed.
- **`sources/lan`**: whatever answers a ping on this machine's subnet. It only ever looks at
  a private subnet of at most 254 addresses. Names come from
  `~/.config/hypertop/lan-names` (an address or MAC, then a name; `-` hides it), then
  multicast DNS, then reverse DNS. Give a machine `-` there if another source already
  shows it.
- **`sources/adb`**: every Android device adb can see, phones and TVs alike: awake or
  asleep, what is in front, battery. `~/.config/hypertop/adb.json` is optional, for adb on
  another machine, or to name a device and say how to wake it; see the top of the file.
- **`sources/pcmlink`**: the streams `pcmlink-mixer` sees, and a way to open it.

The rest of what a person owns (smart plugs, a UPS, a TV) is specific to them, so those
sources live in their own config directory, not here.
