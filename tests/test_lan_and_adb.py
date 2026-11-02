"""What the lan and adb sources read from the network and from adb."""
import importlib.machinery
import importlib.util
import os
import struct
import tempfile
import unittest

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def load(name):
    loader = importlib.machinery.SourceFileLoader(f"{name}_source", os.path.join(HERE, "sources", name))
    spec = importlib.util.spec_from_loader(f"{name}_source", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


lan = load("lan")
adb = load("adb")


class WhatToScan(unittest.TestCase):
    def test_prefix_from_either_tool(self):
        self.assertEqual(lan.parse_prefix("192.168.1.5", "2: eth0    inet 192.168.1.5/24 brd x"), 24)
        self.assertEqual(lan.parse_prefix("192.168.1.5", "inet 192.168.1.5 netmask 0xffffff00 broadcast x"), 24)
        self.assertEqual(lan.parse_prefix("192.168.1.5", "inet 192.168.1.5  netmask 255.255.255.0  broadcast x"), 24)
        self.assertIsNone(lan.parse_prefix("192.168.1.5", "nothing here"))

    def test_a_small_private_network_is_scanned(self):
        hosts = lan.hosts_to_scan("192.168.1.5", 24)
        self.assertEqual(len(hosts), 254)
        self.assertIn("192.168.1.1", hosts)
        self.assertNotIn("192.168.1.0", hosts)
        self.assertNotIn("192.168.1.255", hosts)

    def test_big_or_public_networks_are_left_alone(self):
        self.assertEqual(lan.hosts_to_scan("10.1.2.3", 16), [])
        self.assertEqual(lan.hosts_to_scan("8.8.8.8", 24), [])
        self.assertEqual(lan.hosts_to_scan("192.168.1.5", None), [])
        self.assertEqual(lan.hosts_to_scan("192.168.1.5", 32), [])


class Ping(unittest.TestCase):
    def test_echo_request_has_a_valid_checksum(self):
        packet = lan.echo_request(7)
        self.assertEqual(packet[0], 8)
        self.assertEqual(lan.checksum(packet), 0)


class Mdns(unittest.TestCase):
    def reply(self):
        packet = struct.pack("!6H", 0, 0x8400, 1, 1, 0, 0)
        question = lan.mdns_question("192.0.2.5")
        packet += question[12:]
        packet += b"\xc0\x0c" + struct.pack("!HHIH", lan.TYPE_PTR, 0x8001, 120, 11) + b"\x03box\x05local\x00"
        return packet

    def test_reads_a_reply(self):
        self.assertEqual(lan.ptr_records(self.reply()), {"5.2.0.192.in-addr.arpa": "box.local"})

    def test_garbage_from_the_network_is_only_ever_no_answer(self):
        whole = self.reply()
        for end in range(len(whole)):
            self.assertIsInstance(lan.ptr_records(whole[:end]), dict)
        looped = bytearray(whole)
        looped[12:14] = b"\xc0\x0c"
        self.assertIsInstance(lan.ptr_records(bytes(looped)), dict)


class Names(unittest.TestCase):
    def test_the_names_file(self):
        with tempfile.NamedTemporaryFile("w", delete=False) as fh:
            fh.write("# comment\n192.0.2.1  The router  # upstairs\nAA:BB:CC:DD:EE:FF  Printer\n192.0.2.9 -\n\n")
        self.addCleanup(os.remove, fh.name)
        self.assertEqual(lan.read_names(fh.name), {"192.0.2.1": "The router", "aa:bb:cc:dd:ee:ff": "Printer", "192.0.2.9": "-"})

    def test_a_missing_file_is_no_names(self):
        self.assertEqual(lan.read_names("/nonexistent/names"), {})

    def test_macs_are_written_one_way(self):
        self.assertEqual(lan.normal_mac("A:B:C:D:E:F"), "0a:0b:0c:0d:0e:0f")


class AdbParsing(unittest.TestCase):
    def test_device_list(self):
        text = "List of devices attached\n192.0.2.3:5555\tdevice product:x\nABC\toffline\nDEF\tunauthorized\n* daemon started\n\n"
        self.assertEqual(adb.parse_devices(text), {"192.0.2.3:5555": "device", "ABC": "offline", "DEF": "unauthorized"})

    def test_answers_from_a_phone(self):
        text = ("Some Phone\n16\n  mWakefulness=Awake\n  AC powered: false\n  USB powered: true\n  level: 82\n"
                "  temperature: 312\n  mCurrentFocus=Window{abc u0 com.example.player/x.y}\n")
        a = adb.parse_answers(text)
        self.assertEqual((a["model"], a["android"], a["wakefulness"], a["battery"]), ("Some Phone", "16", "Awake", 82))
        self.assertEqual(a["temp"], 31.2)
        self.assertTrue(a["charging"])
        self.assertEqual(a["app"], "com.example.player")

    def test_answers_from_a_tv_with_little_to_say(self):
        a = adb.parse_answers("Some_TV\n14\n  mWakefulness=Asleep\n")
        self.assertEqual(a["wakefulness"], "Asleep")
        self.assertIsNone(a["battery"])
        self.assertIsNone(a["app"])

    def test_a_command_through_ssh_is_quoted_once_more(self):
        through_ssh = adb.Adb("ssh -T box .local/bin/adb")
        self.assertEqual(through_ssh.shell_argv("s", "a b"), ["ssh", "-T", "box", ".local/bin/adb", "-s", "s", "shell", "'a b'"])
        self.assertEqual(through_ssh.terminal_argv("s"), ["ssh", "-t", "box", ".local/bin/adb", "-s", "s", "shell"])
        local = adb.Adb("adb")
        self.assertEqual(local.shell_argv("s", "a b"), ["adb", "-s", "s", "shell", "a b"])


class AdbCommands(unittest.TestCase):
    def test_what_you_type_goes_through_the_source_and_not_straight_to_adb(self):
        found = adb.actions(adb.Adb("ssh -T box .local/bin/adb"), "s", {})
        run = next(a for a in found if a["id"] == "run")["run"]
        self.assertEqual(run[-3:], ["--run", "s", "{input}"])
        self.assertNotIn("ssh", run)

    def test_through_ssh_all_of_it_reaches_the_device(self):
        argv = adb.command_argv({"adb": "ssh -T box .local/bin/adb"}, "s", "echo ok; id")
        self.assertEqual(argv, ["ssh", "-T", "box", ".local/bin/adb", "-s", "s", "shell", "'echo ok; id'"])

    def test_with_adb_here_nothing_else_reads_it(self):
        self.assertEqual(adb.command_argv({}, "s", "echo ok; id"), ["adb", "-s", "s", "shell", "echo ok; id"])


class PcmlinkFailure(unittest.TestCase):
    def test_a_mixer_that_fails_is_a_failed_source_and_not_an_empty_one(self):
        import subprocess
        import sys

        with tempfile.TemporaryDirectory() as directory:
            mixer = os.path.join(directory, "pcmlink-mixer")
            with open(mixer, "w") as fh:
                fh.write("#!/bin/sh\necho no sockets here >&2\nexit 3\n")
            os.chmod(mixer, 0o755)
            done = subprocess.run([sys.executable, os.path.join(HERE, "sources", "pcmlink")], capture_output=True, text=True, check=False,
                                  env={**os.environ, "PATH": directory + os.pathsep + os.environ["PATH"]})
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("no sockets here", done.stderr)


if __name__ == "__main__":
    unittest.main()
