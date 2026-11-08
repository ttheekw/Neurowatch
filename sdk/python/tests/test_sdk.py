import asyncio
import io
import struct
import unittest

from neurowatch_analytics import Plugin, Session
from neurowatch_analytics.wire import MAX_MESSAGE, GopCache, MediaUnit, encode, read_frame


class WireTests(unittest.TestCase):
    def test_binary_roundtrip_keeps_full_identity_timestamps_and_annexb(self):
        unit = MediaUnit(5, 1 << 48, 1_700_000_000_123_456, b"\0\0\0\1\x65\x12", 3, 123_000, None)
        self.assertEqual(read_frame(io.BytesIO(encode(unit))), unit)
        self.assertEqual(encode({"jsonrpc": "2.0", "id": 1, "result": None})[4], 1)

    def test_lengths_are_refused_before_reading_payloads(self):
        for data in [
            b"\0" * 4,
            struct.pack("<I", MAX_MESSAGE + 2) + b"\1",
            struct.pack("<I", 1) + b"\7",
        ]:
            with self.assertRaises(ValueError):
                read_frame(io.BytesIO(data))
        with self.assertRaises(EOFError):
            read_frame(io.BytesIO(b"\1"))

    def test_fallback_keeps_one_bounded_gop_by_reference(self):
        cache = GopCache()
        key = MediaUnit(1, 1, 1, b"key", 1)
        cache.push(MediaUnit(1, 0, 0, b"delta"))
        self.assertEqual(cache.units, [])
        cache.push(key)
        self.assertIs(cache.units[0], key)
        for seq in range(1024):
            cache.push(MediaUnit(1, seq + 2, 2, b"delta"))
        self.assertEqual(cache.units, [])
        cache.push(MediaUnit(1, 2000, 3, b"delta"))
        self.assertEqual(cache.units, [])
        cache.push(key)
        self.assertEqual(cache.units, [key])


class SessionTests(unittest.IsolatedAsyncioTestCase):
    async def test_control_stays_responsive_while_media_processing_waits(self):
        started = asyncio.Event()
        release = asyncio.Event()

        class Detector(Plugin):
            async def unit(self, unit, session):
                started.set()
                await release.wait()

        session = Session(Detector(), io.BytesIO(), io.BytesIO())
        await session.request("attach", {"binding": 1, "camera": {"id": "c"}, "config": {}})
        worker = session.bindings[1]
        worker.offer(MediaUnit(1, 1, 1, b"picture", 1))
        await asyncio.wait_for(started.wait(), 1)
        self.assertIsNone(await asyncio.wait_for(session.request("ping", {}), 0.1))
        for seq in range(2, 100):
            worker.offer(MediaUnit(1, seq, seq, b"delta"))
        self.assertLessEqual(len(worker.queue), 64)
        self.assertTrue(worker.wait_keyframe)
        await session.request("detach", {"binding": 1})
        self.assertEqual(session.buffered, 0)

    async def test_refusal_and_notifications_use_jsonrpc_without_secret_echo(self):
        from neurowatch_analytics import Refusal

        class Paid(Plugin):
            async def authorize(self, key, credential):
                raise Refusal(
                    "LICENSE_REQUIRED",
                    "A licence is required",
                    url="https://vendor.example/license",
                )

        reader = io.BytesIO(
            encode(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "authenticate",
                    "params": {"public_key": "a", "credential": "private-token"},
                }
            )
        )
        writer = io.BytesIO()
        await Session(Paid(), reader, writer).run()
        response = read_frame(io.BytesIO(writer.getvalue()))
        self.assertEqual(response["error"]["data"]["code"], "LICENSE_REQUIRED")
        self.assertNotIn(b"private-token", writer.getvalue())


if __name__ == "__main__":
    unittest.main()
