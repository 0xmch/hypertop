"""What the ssh source reads from an ssh config."""
import importlib.machinery
import importlib.util
import os
import tempfile
import unittest

PATH = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "sources", "ssh")
loader = importlib.machinery.SourceFileLoader("ssh_source", PATH)
spec = importlib.util.spec_from_loader("ssh_source", loader)
source = importlib.util.module_from_spec(spec)
loader.exec_module(source)


class Wake(unittest.TestCase):
    def test_the_packet_is_six_ffs_and_sixteen_addresses(self):
        packet = source.magic_packet("aa:bb:cc:dd:ee:ff")
        self.assertEqual(len(packet), 102)
        self.assertEqual(packet[:6], b"\xff" * 6)
        self.assertEqual(packet[6:12], bytes.fromhex("aabbccddeeff"))
        self.assertEqual(packet[-6:], bytes.fromhex("aabbccddeeff"))

    def test_any_separator_and_a_bad_address(self):
        self.assertEqual(source.magic_packet("AA-BB-CC-DD-EE-FF"), source.magic_packet("aa:bb:cc:dd:ee:ff"))
        with self.assertRaises(ValueError):
            source.magic_packet("not a mac")

    def test_a_placeholder_address_is_not_an_address(self):
        self.assertIsNone(source.usable_mac("02:00:00:00:00:00"))
        self.assertIsNone(source.usable_mac("00-00-00-00-00-00"))
        self.assertEqual(source.usable_mac("AA-BB-CC-DD-EE-FF"), "aa:bb:cc:dd:ee:ff")

    def test_only_what_a_broadcast_can_reach(self):
        self.assertTrue(source.is_local_network("192.168.1.5"))
        self.assertTrue(source.is_local_network("box.local"))
        self.assertFalse(source.is_local_network("8.8.8.8"))
        self.assertFalse(source.is_local_network("example.com"))


class WhileOff(unittest.TestCase):
    def host(self, hostname):
        host = source.Host.__new__(source.Host)
        host.alias, host.hostname = "box", hostname
        return host

    def test_a_remembered_address_on_this_network_can_be_woken(self):
        actions = self.host("192.168.1.5").actions_while_off("aa:bb:cc:dd:ee:ff")
        self.assertEqual([a["id"] for a in actions], ["wake", "shell"])
        self.assertEqual(actions[0]["run"][-2:], ["--wake", "aa:bb:cc:dd:ee:ff"])

    def test_no_address_or_too_far_away_means_no_wake(self):
        self.assertEqual([a["id"] for a in self.host("192.168.1.5").actions_while_off(None)], ["shell"])
        self.assertEqual([a["id"] for a in self.host("8.8.8.8").actions_while_off("aa:bb:cc:dd:ee:ff")], ["shell"])


class ReadHosts(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.config = os.path.join(self.dir.name, "config")
        source.CONFIG = self.config

    def write(self, name, text):
        with open(os.path.join(self.dir.name, name), "w") as fh:
            fh.write(text)

    def test_aliases_without_patterns(self):
        self.write("config", "# comment\nHost *\n  User x\nHost web www\n  HostName 192.0.2.1\nhost=box\nHost *.lan !bad good\n")
        self.assertEqual(source.read_hosts(self.config), [["web", "www"], ["box"], ["good"]])

    def test_includes_are_followed_and_cannot_loop(self):
        self.write("config", f"Include {self.dir.name}/more*\nHost a\n")
        self.write("more1", f"Include {self.config}\nHost b\n")
        self.assertEqual(source.read_hosts(self.config), [["b"], ["a"]])

    def test_a_missing_config_is_no_hosts(self):
        self.assertEqual(source.read_hosts(os.path.join(self.dir.name, "nope")), [])


if __name__ == "__main__":
    unittest.main()
