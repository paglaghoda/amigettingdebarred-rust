"""Asset compression tests only; never invokes Cargo or builds the web app."""
import contextlib
import gzip
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("build_web", Path(__file__).with_name("build-web.py"))
build_web = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build_web)


class PrecompressionTests(unittest.TestCase):
    def compress(self, assets):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            totals = build_web.precompress_assets(assets)
        return totals, output.getvalue()

    def test_selected_extensions_recursive_and_deterministic(self):
        with tempfile.TemporaryDirectory() as directory:
            assets = Path(directory)
            nested = assets / "nested"
            nested.mkdir()
            data = b"repeatable asset contents\n" * 200
            for extension in ("wasm", "js", "css", "ttf", "svg", "json"):
                (nested / ("app." + extension)).write_bytes(data)
            expected = gzip.compress(data, compresslevel=9, mtime=0)
            totals, output = self.compress(assets)
            self.assertEqual(totals, (len(data), len(expected)))
            self.assertIn(f"{len(data)} bytes uncompressed", output)
            self.assertIn(f"{len(expected)} bytes in stored .gz siblings", output)
            for source in list(nested.iterdir()):
                if source.suffix != ".gz":
                    compressed = source.with_name(source.name + ".gz").read_bytes()
                    self.assertEqual(compressed, expected)
                    self.assertEqual(gzip.decompress(compressed), data)
            self.assertEqual(self.compress(assets)[0], totals)
            self.assertEqual((nested / "app.js.gz").read_bytes(), expected)
            self.assertEqual(list(assets.rglob("*.gz.gz")), [])

    def test_unselected_photos_fonts_and_licenses_untouched(self):
        with tempfile.TemporaryDirectory() as directory:
            assets = Path(directory)
            for name in ("photo.jpg", "photo.png", "photo.webp", "font.woff2", "LICENSE", "font-license.txt"):
                (assets / name).write_bytes(b"compressible" * 200)
            self.assertEqual(self.compress(assets)[0], (0, 0))
            self.assertEqual(list(assets.glob("*.gz")), [])

    def test_nonbeneficial_compression_removes_stale_sibling(self):
        with tempfile.TemporaryDirectory() as directory:
            assets = Path(directory)
            source = assets / "app.js"
            source.write_bytes(b"large repeated contents" * 200)
            self.compress(assets)
            sibling = assets / "app.js.gz"
            self.assertTrue(sibling.exists())
            source.write_bytes(b"x")
            self.compress(assets)
            self.assertFalse(sibling.exists())
            self.assertEqual(source.read_bytes(), b"x")
            # Also succeeds when no obsolete sibling exists.
            self.compress(assets)
            self.assertFalse(sibling.exists())

    def test_wasm_totals_include_all_sources_but_only_beneficial_siblings(self):
        with tempfile.TemporaryDirectory() as directory:
            assets = Path(directory)
            data = b"wasm payload" * 300
            (assets / "app.wasm").write_bytes(data)
            (assets / "tiny.wasm").write_bytes(b"x")
            (assets / "boot.js").write_bytes(data)
            totals, _ = self.compress(assets)
            self.assertEqual(totals, (len(data) + 1, len(gzip.compress(data, compresslevel=9, mtime=0))))
            self.assertFalse((assets / "tiny.wasm.gz").exists())


if __name__ == "__main__":
    unittest.main()
