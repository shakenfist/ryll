#!/usr/bin/env python3
"""Unit tests for tools/rdp-smoke.py's pure parts.

The RDP smoke lane needs QEMU, Xvfb and FreeRDP, so it cannot run from a
pre-commit hook the way tools/test-proxmox-smoke.sh runs its driver against
a fake ryll: there is no cheap fake for an X server and an RDP client.
What can be pinned without them is what decides pass and fail: reading a
pixel out of xwd's and QEMU's dumps, the colour comparisons, ryll's output
lines the checks match, the password scrub and the summary lines. A bug in
any of those reads as a green lane or a red one for the wrong reason.

The log line fixtures are copied from a real run's ryll output; if ryll
rewords a line, update the driver's constant (whose comment names the
source line) and the fixture here together.

No network, no Docker. Python 3 standard library only.

Usage: tools/test-rdp-smoke.py
"""

import contextlib
import importlib.util
import io
import os
import struct
import unittest


HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location('rdp_smoke', os.path.join(HERE, 'rdp-smoke.py'))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


def make_xwd(width, height, colour_at, byte_order=0, bits_per_pixel=32, visual_class=4, ncolors=2,
             name=b'rdp-smoke-freerdp-1\x00', masks=(0xff0000, 0xff00, 0xff)):
    """An xwd dump as xwd -silent writes one for a 24-bit TrueColor window."""
    step = bits_per_pixel // 8
    bytes_per_line = width * step
    header_size = 100 + len(name)
    fields = [
        header_size, 7, 2, 24, width, height, 0, byte_order, 32, byte_order, 32, bits_per_pixel,
        bytes_per_line, visual_class, masks[0], masks[1], masks[2], 8, 256, ncolors, width, height, 0, 0, 0,
    ]
    out = bytearray(struct.pack('>25I', *fields) + name)
    out += b'\xaa' * (12 * ncolors)
    order = 'little' if byte_order == 0 else 'big'
    for y in range(height):
        for x in range(width):
            r, g, b = colour_at(x, y)
            pixel = 0
            for value, mask in zip((r, g, b), masks):
                shift = (mask & -mask).bit_length() - 1
                pixel |= (value << shift) & mask
            out += pixel.to_bytes(step, order)
    return bytes(out)


def centre_marked(width, height, colour):
    def colour_at(x, y):
        return colour if (x, y) == (width // 2, height // 2) else (1, 2, 3)
    return colour_at


class XwdTest(unittest.TestCase):
    def test_reads_the_centre_pixel_lsb_first(self):
        data = make_xwd(8, 6, centre_marked(8, 6, (0x96, 0x00, 0x97)))
        self.assertEqual(smoke.xwd_centre(data), (8, 6, (0x96, 0x00, 0x97)))

    def test_reads_the_centre_pixel_msb_first(self):
        data = make_xwd(7, 5, centre_marked(7, 5, (0x10, 0x98, 0x20)), byte_order=1)
        self.assertEqual(smoke.xwd_centre(data), (7, 5, (0x10, 0x98, 0x20)))

    def test_reads_24_bit_pixels(self):
        data = make_xwd(4, 4, centre_marked(4, 4, (0xff, 0x80, 0x01)), bits_per_pixel=24)
        self.assertEqual(smoke.xwd_centre(data)[2], (0xff, 0x80, 0x01))

    def test_skips_the_colour_map(self):
        data = make_xwd(4, 4, centre_marked(4, 4, (9, 8, 7)), ncolors=256)
        self.assertEqual(smoke.xwd_centre(data)[2], (9, 8, 7))

    def test_scales_a_narrow_mask_to_eight_bits(self):
        masks = (0xf800, 0x07e0, 0x001f)
        data = make_xwd(2, 2, lambda x, y: (31, 63, 0), bits_per_pixel=32, masks=masks)
        self.assertEqual(smoke.xwd_centre(data)[2], (255, 255, 0))

    def test_refuses_a_pseudocolor_dump(self):
        data = make_xwd(2, 2, lambda x, y: (0, 0, 0), visual_class=3)
        with self.assertRaises(ValueError):
            smoke.xwd_centre(data)

    def test_refuses_a_truncated_dump(self):
        data = make_xwd(4, 4, lambda x, y: (0, 0, 0))
        with self.assertRaises(ValueError):
            smoke.xwd_centre(data[:120])
        with self.assertRaises(ValueError):
            smoke.xwd_centre(data[:50])


class PpmTest(unittest.TestCase):
    def ppm(self, width, height, colour, header=None):
        pixels = bytearray()
        for y in range(height):
            for x in range(width):
                pixels += bytes(colour if (x, y) == (width // 2, height // 2) else (5, 5, 5))
        head = header or f'P6\n{width} {height}\n255\n'.encode()
        return head + bytes(pixels)

    def test_reads_the_centre_pixel(self):
        self.assertEqual(smoke.ppm_centre(self.ppm(6, 4, (0, 0, 0x98))), (6, 4, (0, 0, 0x98)))

    def test_skips_comments(self):
        data = self.ppm(3, 3, (1, 2, 3), header=b'P6\n# made by a test\n3 3\n255\n')
        self.assertEqual(smoke.ppm_centre(data)[2], (1, 2, 3))

    def test_refuses_other_formats(self):
        with self.assertRaises(ValueError):
            smoke.ppm_centre(b'P3\n1 1\n255\n0 0 0\n')

    def test_refuses_a_truncated_dump(self):
        with self.assertRaises(ValueError):
            smoke.ppm_centre(self.ppm(6, 4, (1, 2, 3))[:20])


class ColourTest(unittest.TestCase):
    def test_black(self):
        self.assertTrue(smoke.is_black((0, 0, 0)))
        self.assertTrue(smoke.is_black((10, 20, 24)))
        self.assertFalse(smoke.is_black((0, 0, 0x97)))
        # Half of the guest's dimmest colour is not black.
        self.assertFalse(smoke.is_black((0, 0, 0x96 // 2)))

    def test_same_colour_allows_rounding(self):
        self.assertTrue(smoke.same_colour((0, 0, 0x97), (0, 0, 0x98)))
        self.assertFalse(smoke.same_colour((0, 0, 0x97), (0, 0x98, 0x97)))
        self.assertFalse(smoke.same_colour((0, 0, 0), (0, 0, 0x97)))

    def test_darkened_is_half_brightness(self):
        # Seen in a real run: 0x96 red darkened to 0x49 after the codec.
        self.assertTrue(smoke.is_darkened((0x96, 0, 0), (0x49, 0, 0)))
        self.assertTrue(smoke.is_darkened((0x96, 0x97, 0), (0x4b, 0x4b, 0)))
        # The undarkened frame is not darkened, nor is black.
        self.assertFalse(smoke.is_darkened((0x96, 0, 0), (0x96, 0, 0)))
        self.assertFalse(smoke.is_darkened((0x96, 0x98, 0x97), (0, 0, 0)))
        # Another colour at half brightness is not this one darkened.
        self.assertFalse(smoke.is_darkened((0x96, 0, 0), (0, 0x4b, 0)))

    def test_colour_format(self):
        self.assertEqual(smoke.fmt_colour((0x96, 0, 0x0a)), '#96000a')
        self.assertEqual(smoke.fmt_colour(None), 'none')


# Lines from a real run of ryll --rdp, timestamps and all.
RYLL_OUTPUT = """RDP: 127.0.0.1:42705
user: ryll
password: rdp-smoke-test-password
security: tls
certificate SHA-256: 00:11
2026-10-26T09:31:56.289446Z  INFO rdp: SPICE attempt 1: connecting
2026-10-26T09:31:56.626402Z  INFO RDP client authenticated username=ryll session=127.0.0.1:42023
2026-10-26T09:32:00.762543Z  INFO RDP client authenticated username=ryll session=127.0.0.1:42023
2026-10-26T09:32:04.906596Z  INFO RDP client took the session over from the client being served \
peer=127.0.0.1:39708 previous=127.0.0.1:39700 session=127.0.0.1:42023
2026-10-26T09:32:09.487941Z  INFO rdp: SPICE attempt 1: the session ended
2026-10-26T09:32:10.489584Z  INFO rdp: SPICE attempt 2: connecting
2026-10-26T09:32:12.000000Z  WARN RDP client presented wrong credentials username=ryll
"""


class RyllOutputTest(unittest.TestCase):
    lines = RYLL_OUTPUT.splitlines()

    def test_finds_the_rdp_address(self):
        self.assertEqual(smoke.rdp_address(self.lines), ('127.0.0.1', 42705))
        self.assertIsNone(smoke.rdp_address(self.lines[1:]))

    def test_counts_authentications_not_refusals(self):
        self.assertEqual(smoke.count_matching(self.lines, smoke.AUTHENTICATED_RE), 2)

    def test_counts_the_takeover(self):
        self.assertEqual(smoke.count_matching(self.lines, smoke.TAKEOVER_RE), 1)

    def test_counts_attempts_not_their_ends(self):
        self.assertEqual(smoke.count_matching(self.lines, smoke.ATTEMPT_RE), 2)


class PasswordTest(unittest.TestCase):
    def test_ryll_may_print_it_on_its_launch_line(self):
        self.assertEqual(smoke.password_leaks('ryll', RYLL_OUTPUT), 0)

    def test_ryll_may_not_print_it_anywhere_else(self):
        text = RYLL_OUTPUT + f'2026-10-26T09:32:13Z DEBUG credentials password={smoke.PASSWORD}\n'
        self.assertEqual(smoke.password_leaks('ryll', text), 1)

    def test_a_client_may_not_print_it_at_all(self):
        self.assertEqual(smoke.password_leaks('freerdp-1', f'password: {smoke.PASSWORD}\n'), 1)

    def test_redaction(self):
        text, count = smoke.redact(RYLL_OUTPUT, smoke.PASSWORD)
        self.assertEqual(count, 1)
        self.assertNotIn(smoke.PASSWORD, text)
        self.assertIn('password: <redacted>', text)


class SummaryTest(unittest.TestCase):
    def test_pass(self):
        line = smoke.summary_line('input', 'PASS', 1.25, [], ['key moved', 'twice'])
        self.assertEqual(line, '[rdp-smoke] input: PASS in 1.2s -- key moved; twice')

    def test_fail_carries_problems_then_notes(self):
        line = smoke.summary_line('takeover', 'FAIL', 20.0, ['still connected'], ['authenticated'])
        self.assertEqual(line, '[rdp-smoke] takeover: FAIL in 20.0s -- still connected [authenticated]')

    def test_harness_failure_is_marked(self):
        line = smoke.summary_line('setup', 'HARNESS', 3.0, ['HARNESS: no Xvfb'], [])
        self.assertTrue(line.startswith('[rdp-smoke] setup: FAIL (harness) in 3.0s -- HARNESS: no Xvfb'))

    def test_skip(self):
        self.assertEqual(smoke.summary_line('ctrl-c', 'SKIP', 0, [], []),
                         '[rdp-smoke] ctrl-c: SKIP -- an earlier check failed')


class ArgsTest(unittest.TestCase):
    def args(self, *extra):
        return smoke.parse_args(['--ryll', '/bin/true', '--guest', '/dev/null', '--workdir', '/tmp', *extra])

    def test_every_check_by_default_in_order(self):
        self.assertEqual(self.args().checks, list(smoke.CHECKS))

    def test_a_selection_runs_everything_up_to_its_last_check(self):
        self.assertEqual(self.args('--checks', 'input').checks, ['authenticate', 'frame', 'input'])

    def test_an_unknown_check_is_a_usage_error(self):
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            self.args('--checks', 'nonsense')


if __name__ == '__main__':
    unittest.main()
