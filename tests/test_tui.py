"""hypertop itself, driven through a terminal: what you press against what you see."""
import codecs
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import stat
import struct
import subprocess
import tempfile
import termios
import time
import unittest

BINARY = os.environ.get("HYPERTOP_BIN") or os.path.join(os.path.dirname(__file__), "..", "target", "release", "hypertop")
DOWN, UP, RIGHT, LEFT = b"\x1b[B", b"\x1b[A", b"\x1b[C", b"\x1b[D"
SELECTED = 238  # the background of the selected row
TOKEN = re.compile(r"\x1b\[([?<]?)([0-9;]*)([@-~])")


class Screen:
    """Enough of a terminal for what ratatui writes."""

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self.pending = ""
        self.clear()
        self.y = self.x = 0
        self.bg = None

    def clear(self):
        self.cells = [[(" ", None) for _ in range(self.cols)] for _ in range(self.rows)]

    def feed(self, data):
        text = self.pending + self.decoder.decode(data)
        self.pending = ""
        at = 0
        while at < len(text):
            if text[at] != "\x1b":
                if text[at] >= " " and self.y < self.rows and self.x < self.cols:
                    self.cells[self.y][self.x] = (text[at], self.bg)
                    self.x += 1
                at += 1
                continue
            token = TOKEN.match(text, at)
            if not token:
                self.pending = text[at:]
                return
            self.csi(token.group(1), token.group(2), token.group(3))
            at = token.end()

    def csi(self, private, params, final):
        if private:
            if (private, params, final) == ("?", "1049", "h"):  # a blank alternate screen
                self.clear()
            return
        if final == "H":
            parts = params.split(";") if params else ["1", "1"]
            self.y, self.x = int(parts[0]) - 1, int(parts[1]) - 1
        elif final == "J":
            self.clear()
        elif final == "m":
            numbers = [int(p or 0) for p in params.split(";")]
            i = 0
            while i < len(numbers):
                if numbers[i] == 48 and numbers[i + 1 : i + 2] == [5]:
                    self.bg = numbers[i + 2]
                    i += 3
                    continue
                if numbers[i] in (38, 58):
                    i += 3
                    continue
                if numbers[i] in (0, 49):
                    self.bg = None
                i += 1

    def text(self):
        return "\n".join("".join(c for c, _ in row).rstrip() for row in self.cells)

    def selected(self):
        for row in self.cells:
            if any(bg == SELECTED for _, bg in row):
                return "".join(c for c, _ in row).strip()
        return None


class Tui:
    def __init__(self, sources, rows=40, cols=150):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ["TERM"] = "xterm-256color"
            os.execv(BINARY, [BINARY, "--sources", sources])
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.screen = Screen(rows, cols)
        self.raw = b""
        self.rusage = None

    def read(self, seconds):
        """Keep the screen up to date for this long."""
        end = time.perf_counter() + seconds
        while (left := end - time.perf_counter()) > 0:
            if select.select([self.fd], [], [], left)[0]:
                try:
                    data = os.read(self.fd, 1 << 16)
                except OSError:
                    return
                if not data:
                    return
                self.raw += data
                self.screen.feed(data)

    def until(self, what, seconds=5.0):
        """Wait for something to be true of the screen."""
        end = time.perf_counter() + seconds
        while time.perf_counter() < end:
            if what(self.screen):
                return True
            self.read(0.01)
        return what(self.screen)

    def press(self, key):
        os.write(self.fd, key)

    def latency(self, key):
        """Milliseconds from pressing a key to the screen starting to change."""
        self.press(key)
        start = time.perf_counter()
        if not select.select([self.fd], [], [], 1.0)[0]:
            return 1000.0
        data = os.read(self.fd, 1 << 16)
        took = (time.perf_counter() - start) * 1000
        self.screen.feed(data)
        return took

    def resize(self, rows, cols):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)
        self.screen = Screen(rows, cols)

    def quit(self, key=b"q"):
        """True if it left when asked."""
        if self.rusage is not None:
            return True
        self.press(key)
        end = time.time() + 3
        while time.time() < end:
            self.read(0.02)
            pid, _, usage = os.wait4(self.pid, os.WNOHANG)
            if pid:
                self.rusage = usage
                os.close(self.fd)
                return True
        os.kill(self.pid, signal.SIGKILL)
        self.rusage = os.wait4(self.pid, 0)[2]
        os.close(self.fd)
        return False

    def cpu_seconds(self):
        return self.rusage.ru_utime + self.rusage.ru_stime


def device(i, **more):
    return {
        "id": f"d{i}",
        "name": f"device {i:04}",
        "group": f"group {i // 10}",
        "kind": "odd" if i % 2 else "even",
        "state": "down" if i % 5 == 4 else "up",
        "note": f"note-{i}",
        "metrics": {"cpu": i % 100, "mem": 40},
        "actions": [{"id": "shell", "label": "Shell", "run": ["sh", "-c", "echo handed over; sleep 0.2"], "tty": True}],
        **more,
    }


class Case(unittest.TestCase):
    every = 3600

    def setUp(self):
        if not os.path.exists(BINARY):
            self.skipTest("cargo build --release first")
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.sources = os.path.join(self.dir, "sources")
        os.mkdir(self.sources)
        self.list = os.path.join(self.dir, "devices")
        self.peeks = os.path.join(self.dir, "peeks")
        script = os.path.join(self.sources, "fake")
        with open(script, "w") as f:
            f.write(f'#!/bin/sh\necho \'{{"every": {self.every}}}\'\ncat "{self.list}"\n')
        os.chmod(script, os.stat(script).st_mode | stat.S_IEXEC)

    def describe(self, devices):
        with open(self.list + ".new", "w") as f:
            f.writelines(json.dumps(d) + "\n" for d in devices)
        os.replace(self.list + ".new", self.list)

    def start(self, count, **kwargs):
        peek = lambda i: [{"label": "look", "run": ["sh", "-c", f"echo {i} >> {self.peeks}; echo answer-{i}"], "every": 600}]
        self.describe(device(i, peeks=peek(i)) for i in range(count))
        tui = Tui(self.sources, **kwargs)
        self.addCleanup(tui.quit)
        self.assertTrue(tui.until(lambda s: f"device {count - 1:04}" in s.text() or "device 00" in s.text()), tui.screen.text())
        tui.read(0.2)
        return tui


class Responsiveness(Case):
    def test_a_key_reaches_the_screen_without_waiting_for_a_frame(self):
        tui = self.start(30)
        taken = []
        for i in range(40):
            taken.append(tui.latency(DOWN if i % 16 < 8 else UP))
            tui.read(0.012 + (i % 7) * 0.01)
        taken.sort()
        self.assertLess(taken[len(taken) // 2], 20, taken)

    def test_many_devices_do_not_slow_a_key_down(self):
        tui = self.start(3000)
        taken = []
        for i in range(30):
            taken.append(tui.latency(DOWN if i % 10 < 5 else UP))
            tui.read(0.02)
        taken.sort()
        self.assertLess(taken[len(taken) // 2], 30, taken)

    def test_holding_a_key_moves_at_the_speed_it_repeats(self):
        tui = self.start(30)
        updates, sent, last = 0, 0, 0.0
        start = time.perf_counter()
        while time.perf_counter() - start < 1.5:
            tui.press(DOWN if sent % 32 < 16 else UP)
            sent += 1
            end = start + sent / 30
            while (left := end - time.perf_counter()) > 0:
                if select.select([tui.fd], [], [], left)[0]:
                    tui.screen.feed(os.read(tui.fd, 1 << 16))
                    if time.perf_counter() - last > 0.004:
                        updates += 1
                    last = time.perf_counter()
        self.assertGreater(updates, sent * 0.8, f"{updates} updates for {sent} keys")

    def test_a_burst_of_keys_lands_where_it_should(self):
        tui = self.start(40)
        tui.press(DOWN * 15)
        self.assertTrue(tui.until(lambda s: "device 0015" in (s.selected() or ""), 1.0), tui.screen.selected())

    def test_nothing_happening_costs_next_to_nothing(self):
        tui = self.start(1000)
        tui.read(3)
        self.assertTrue(tui.quit())
        self.assertLess(tui.cpu_seconds(), 0.3)


class Peeks(Case):
    def test_passing_over_devices_asks_only_the_one_you_stop_at(self):
        tui = self.start(40)
        tui.press(RIGHT)
        self.assertTrue(tui.until(lambda s: "answer-0" in s.text()), tui.screen.text())
        for _ in range(20):
            tui.press(DOWN)
            tui.read(0.03)
        self.assertTrue(tui.until(lambda s: "answer-20" in s.text()), tui.screen.text())
        with open(self.peeks) as f:
            self.assertEqual(f.read().split(), ["0", "20"])

    def test_the_answer_shows_without_touching_a_key(self):
        tui = self.start(5)
        tui.press(RIGHT)
        self.assertTrue(tui.until(lambda s: "answer-0" in s.text(), 2.0), tui.screen.text())


class Views(Case):
    """What changes the list, with the screen as the judge."""

    def test_filter_narrows_at_each_key_and_esc_brings_all_back(self):
        tui = self.start(30)
        tui.press(b"/")
        tui.press(b"note-17")
        self.assertTrue(tui.until(lambda s: "device 0017" in s.text() and "device 0003" not in s.text(), 1.0), tui.screen.text())
        self.assertIn("device 0017", tui.screen.selected() or "")
        tui.press(b"\r")
        tui.read(0.05)
        tui.press(b"\x1b")
        self.assertTrue(tui.until(lambda s: "device 0003" in s.text(), 2.0), tui.screen.text())

    def test_hiding_what_is_off_and_regrouping_redraw_at_once(self):
        tui = self.start(10)
        self.assertIn("device 0004", tui.screen.text())
        tui.press(b"d")
        self.assertTrue(tui.until(lambda s: "device 0004" not in s.text(), 1.0), tui.screen.text())
        tui.press(b"d")
        self.assertTrue(tui.until(lambda s: "device 0004" in s.text(), 1.0))
        self.assertIn(" group 0 ", tui.screen.text())
        tui.press(b"b")
        self.assertTrue(tui.until(lambda s: " odd " in s.text() and " group 0 " not in s.text(), 1.0), tui.screen.text())

    def test_moving_through_a_regrouped_list_follows_the_new_order(self):
        tui = self.start(6)
        tui.press(b"b")
        tui.until(lambda s: " odd " in s.text(), 1.0)
        tui.press(DOWN)
        self.assertTrue(tui.until(lambda s: "device 0002" in (s.selected() or ""), 1.0), tui.screen.text())

    def test_the_selection_moves_on_when_its_device_goes_away(self):
        tui = self.start(6)
        for _ in range(3):
            tui.press(DOWN)
        self.assertTrue(tui.until(lambda s: "device 0003" in (s.selected() or ""), 1.0))
        self.describe(device(i) for i in (0, 1, 2, 4, 5))
        tui.press(b"r")
        self.assertTrue(tui.until(lambda s: "device 0003" not in s.text(), 3.0), tui.screen.text())
        self.assertIn("device 0000", tui.screen.selected() or "")
        tui.press(DOWN)
        self.assertTrue(tui.until(lambda s: "device 0001" in (s.selected() or ""), 1.0), "the rest still answer")

    def test_a_resize_redraws(self):
        tui = self.start(6)
        tui.resize(30, 100)
        self.assertTrue(tui.until(lambda s: "device 0005" in s.text(), 2.0), tui.screen.text())

    def test_the_terminal_comes_back_from_a_shell(self):
        tui = self.start(6)
        tui.press(b"s")
        self.assertTrue(tui.until(lambda s: tui.raw.count(b"\x1b[?1000h") == 2, 3.0), "it never took the terminal back")
        self.assertIn(b"handed over", tui.raw)
        tui.read(0.1)
        self.assertIn("device 0005", tui.screen.text())
        self.assertIn("device 0000", tui.screen.selected() or "")
        tui.press(DOWN)
        self.assertTrue(tui.until(lambda s: "device 0001" in (s.selected() or ""), 1.0))


class Actions(Case):
    every = 1

    def mark(self, i, text="", **more):
        marks = os.path.join(self.dir, "marks")
        return {"id": "off", "label": "Turn off", "run": ["sh", "-c", f"echo {text}{i} >> {marks}"], **more}

    def marked(self):
        try:
            with open(os.path.join(self.dir, "marks")) as f:
                return sorted(f.read().split())
        except OSError:
            return []

    def begin(self, devices):
        self.describe(devices)
        tui = Tui(self.sources)
        self.addCleanup(tui.quit)
        self.assertTrue(tui.until(lambda s: "device 0000" in s.text()), tui.screen.text())
        tui.read(0.2)
        return tui

    def test_what_you_confirm_runs_where_you_confirmed_it_whatever_the_list_does(self):
        tui = self.begin(device(i, actions=[self.mark(i, confirm=True)]) for i in range(4))
        tui.press(DOWN)
        self.assertTrue(tui.until(lambda s: "device 0001" in (s.selected() or ""), 1.0))
        tui.press(b"\r")
        self.assertTrue(tui.until(lambda s: "act on device 0001" in s.text(), 1.0), tui.screen.text())
        tui.press(b"\r")
        self.assertTrue(tui.until(lambda s: "are you sure" in s.text(), 1.0), tui.screen.text())
        self.describe(device(i, actions=[self.mark(i, confirm=True)]) for i in (0, 2, 3))
        self.assertTrue(tui.until(lambda s: "note-1" not in s.text(), 4.0), tui.screen.text())
        tui.press(b"y")
        tui.until(lambda s: self.marked(), 2.0)
        self.assertEqual(self.marked(), ["1"])

    def test_a_batch_goes_to_everything_tagged_even_what_a_filter_hides(self):
        tui = self.begin(device(i, actions=[self.mark(i, confirm=True)]) for i in range(4))
        tui.press(b"  ")
        tui.press(b"/note-3\r")
        self.assertTrue(tui.until(lambda s: "note-0" not in s.text() and "2 tagged" in s.text(), 1.0), tui.screen.text())
        tui.press(b"\r")
        self.assertTrue(tui.until(lambda s: "act on 2 devices" in s.text(), 1.0), tui.screen.text())
        tui.press(b"\r")
        self.assertTrue(tui.until(lambda s: "are you sure" in s.text(), 1.0))
        tui.press(b"y")
        tui.read(0.3)
        self.assertEqual(self.marked(), [], "several devices take a capital Y")
        tui.press(b"\r")
        tui.until(lambda s: "act on 2 devices" in s.text(), 1.0)
        tui.press(b"\r")
        tui.until(lambda s: "are you sure" in s.text(), 1.0)
        tui.press(b"Y")
        tui.until(lambda s: len(self.marked()) == 2, 2.0)
        self.assertEqual(self.marked(), ["0", "1"])

    def test_a_shell_is_for_the_device_under_the_cursor_whatever_is_tagged(self):
        def shell(i):
            return {"id": "shell", "label": "Shell", "run": ["sh", "-c", f"echo shell-{i}; sleep 0.2"], "tty": True}

        tui = self.begin(device(i, actions=[shell(i)]) for i in range(4))
        tui.press(b" ")
        tui.press(DOWN)
        self.assertTrue(tui.until(lambda s: "device 0002" in (s.selected() or ""), 1.0))
        tui.press(b"s")
        self.assertTrue(tui.until(lambda s: tui.raw.count(b"\x1b[?1000h") == 2, 3.0))
        self.assertIn(b"shell-2", tui.raw)
        self.assertNotIn(b"shell-0", tui.raw)
        self.assertTrue(tui.until(lambda s: "1 tagged" in s.text(), 1.0), "the tag stays")

    def test_a_batch_asks_for_text_if_any_of_it_needs_it_whichever_comes_first(self):
        tui = self.begin([device(0, actions=[{**self.mark(0, "{input}"), "id": "run"}]), device(1, actions=[{**self.mark(1, "{input}"), "id": "run", "input": "Text"}])])
        tui.press(b"  ")
        self.assertTrue(tui.until(lambda s: "2 tagged" in s.text(), 1.0))
        tui.press(b"!")
        self.assertTrue(tui.until(lambda s: "Text, on 2 devices" in s.text(), 1.0), tui.screen.text())
        tui.press(b"hi-\r")
        tui.until(lambda s: len(self.marked()) == 2, 2.0)
        self.assertEqual(self.marked(), ["hi-0", "hi-1"])


class Failures(Case):
    def test_a_source_printing_nonsense_keeps_its_devices_and_says_so(self):
        tui = self.start(3)
        with open(self.list, "a") as f:
            f.write("this is not json\n")
        tui.press(b"r")
        self.assertTrue(tui.until(lambda s: "isn't JSON" in s.text(), 3.0), tui.screen.text())
        self.assertIn("device 0002", tui.screen.text())
        self.describe(device(i) for i in range(2))
        tui.press(b"r")
        self.assertTrue(tui.until(lambda s: "isn't JSON" not in s.text() and "device 0002" not in s.text(), 3.0), tui.screen.text())

    def test_bytes_that_are_not_text_do_not_stop_a_source_being_read(self):
        tui = self.start(2)
        with open(self.list, "ab") as f:
            f.write(b"\xff\xfe\n" + json.dumps(device(9)).encode() + b"\n")
        tui.press(b"r")
        self.assertTrue(tui.until(lambda s: "device 0009" in s.text(), 3.0), tui.screen.text())

    def test_the_list_on_stdout_carries_no_terminal_codes(self):
        self.describe([device(0, name="\x1b[31mred\x1b]0;title\x07", note="\x1b[2Jnote")])
        done = subprocess.run([BINARY, "--list", "--sources", self.sources, "--wait", "1"], capture_output=True, text=True, check=False)
        self.assertIn("red", done.stdout)
        self.assertNotIn("\x1b", done.stdout)


class PeekOwnership(Case):
    def begin(self):
        self.pidfile = os.path.join(self.dir, "pid")
        slow = [{"label": "slow", "run": ["sh", "-c", f"echo $$ > {self.pidfile}; sleep 30"], "every": 600}]
        self.describe(device(i, peeks=slow) for i in range(3))
        tui = Tui(self.sources)
        self.addCleanup(tui.quit)
        self.assertTrue(tui.until(lambda s: "device 0002" in s.text()))
        tui.read(0.2)
        tui.press(RIGHT)
        self.assertTrue(tui.until(lambda s: os.path.exists(self.pidfile) and os.path.getsize(self.pidfile) > 0, 2.0), "the peek never started")
        with open(self.pidfile) as f:
            self.pid = int(f.read())
        return tui

    def running(self):
        try:
            os.kill(self.pid, 0)
            return True
        except ProcessLookupError:
            return False

    def ends(self, tui):
        end = time.time() + 2
        while time.time() < end and self.running():
            tui.read(0.02)
        return not self.running()

    def test_it_stops_when_you_move_to_another_device(self):
        tui = self.begin()
        tui.press(DOWN)
        self.assertTrue(self.ends(tui))

    def test_it_stops_when_you_leave_the_tab(self):
        tui = self.begin()
        tui.press(LEFT)
        self.assertTrue(self.ends(tui))

    def test_it_stops_when_you_hide_the_pane(self):
        tui = self.begin()
        tui.press(b"\t")
        self.assertTrue(self.ends(tui))

    def test_it_stops_when_the_output_view_covers_it(self):
        tui = self.begin()
        tui.press(b"o")
        self.assertTrue(self.ends(tui))

    def test_it_stops_when_hypertop_leaves(self):
        tui = self.begin()
        self.assertTrue(tui.quit())
        end = time.time() + 2
        while time.time() < end and self.running():
            time.sleep(0.02)
        self.assertFalse(self.running())


class Mouse(Case):
    def test_only_what_is_used_is_asked_for(self):
        tui = self.start(6)
        self.assertIn(b"\x1b[?1000h", tui.raw)
        self.assertIn(b"\x1b[?1006h", tui.raw)
        self.assertNotIn(b"\x1b[?1003h", tui.raw, "every movement of the mouse")
        self.assertTrue(tui.quit())
        self.assertIn(b"\x1b[?1000l", tui.raw)

    def test_the_wheel_moves_and_a_click_selects(self):
        tui = self.start(8)
        tui.press(b"\x1b[<65;20;6M")
        self.assertTrue(tui.until(lambda s: "device 0001" in (s.selected() or ""), 1.0), tui.screen.selected())
        row = next(i for i, line in enumerate(tui.screen.text().split("\n")) if "device 0005" in line)
        tui.press(f"\x1b[<0;20;{row + 1}M\x1b[<0;20;{row + 1}m".encode())
        self.assertTrue(tui.until(lambda s: "device 0005" in (s.selected() or ""), 1.0), tui.screen.selected())


class Leaving(Case):
    def test_q_and_ctrl_c_both_leave(self):
        self.assertTrue(self.start(3).quit(b"q"))
        self.assertTrue(self.start(3).quit(b"\x03"))


if __name__ == "__main__":
    unittest.main()
