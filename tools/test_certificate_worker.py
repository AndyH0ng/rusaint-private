"""과금·출력 없이 설정 오류, 파서 호환성과 발급 지연을 검증한다."""

import json
from pathlib import Path
import tempfile
import unittest
import zlib
from unittest.mock import Mock, patch

from certificate_document import issue
from certificate_worker import KINDS, Worker


class WorkerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.worker = Worker()
        self.worker.root = Path(self.temp.name)
        self.worker.assets = self.worker.root / "assets"
        self.worker.system_ip = "192.0.2.1"
        self.worker.school = Mock()
        self.preview = {"minno": "new-doc", "job": KINDS["scholarship"], "count": 1,
                        "printed": 0, "printgb": "1", "printable": "Y", "korean": "Y"}

    def test_missing_or_invalid_ip_does_not_touch_document(self):
        for address in ("", "not-an-ip"):
            self.worker.system_ip = address
            with self.assertRaises(ValueError):
                self.worker.pdf({"document_id": "new-doc"})
            self.worker.school.post.assert_not_called()
            self.assertFalse((self.worker.root / "pdf-new-doc").exists())

    def test_common_issue_validates_ip_before_creating_directory(self):
        output = self.worker.root / "output"
        with self.assertRaises(ValueError):
            issue(self.worker.school, self.preview, exe=Path("missing"), fonts=Path("missing"),
                  output_dir=output, system_ip="invalid")
        self.assertFalse(output.exists())
        self.worker.school.post.assert_not_called()

    def test_reportx_preview_appears_in_worker_list(self):
        self.worker.school.post.return_value = b"""<a href="javascript:logout();">logout</a>
          <tr><td><a href="javascript:reportx.preview('cert','new-doc','null','2','1','Y','1','Y')">print</a></td></tr>"""
        self.assertEqual(self.worker.list({})["items"], [{"document_id": "new-doc", "name": "cert",
                                                         "copies": 2, "printed": 1, "available": True}])

    def test_worker_uses_common_issue_and_reuses_verified_cache(self):
        def render_once(school, preview, **settings):
            cache = settings["output_dir"]
            cache.mkdir()
            (cache / "certificate.pdf").write_bytes(b"%PDF-1.7\nexample")
            metadata = {"server_verification_success": True, "print_completion_sent": True}
            (cache / "metadata.json").write_text(json.dumps(metadata))
            return cache / "certificate.pdf", metadata
        with patch.object(self.worker, "native_basket", return_value={"new-doc": self.preview}), \
                patch("certificate_worker.issue", side_effect=render_once) as common:
            first = self.worker.pdf({"document_id": "new-doc"})
            second = self.worker.pdf({"document_id": "new-doc"})
        self.assertEqual(first, second)
        common.assert_called_once()

    def test_unverified_cache_is_not_published_or_reprinted(self):
        cache = self.worker.root / "pdf-new-doc"
        cache.mkdir()
        (cache / "metadata.json").write_text(json.dumps({"server_verification_success": False,
                                                        "print_completion_sent": True}))
        with patch("certificate_worker.issue") as common:
            result = self.worker.pdf({"document_id": "new-doc"})
        self.assertEqual(result["status"], "verification_pending")
        self.assertNotIn("pdf_base64", result)
        common.assert_not_called()
        self.worker.school.post.assert_not_called()

    def test_common_pipeline_preserves_failed_verification_as_metadata(self):
        output = self.worker.root / "document"
        cipher = Mock()
        launch = ("http://127.0.0.1/SHOWREPORT?MINNO=new-doc|TPID=tp|RECEIVE_TYPE=WEB|"
                  "URLFile=certi.ssu.ac.kr/wm1.0/jsp/report/sendfile.jsp|"
                  "URLCheck=certi.ssu.ac.kr/wm1.0/jsp/report/senddocno.jsp|"
                  "URLPost=certi.ssu.ac.kr/wm1.0/jsp/report/printcomplete.jsp").encode()
        cipher.decrypt.side_effect = [launch, zlib.compress(b"prepared report")]
        self.worker.school.post.side_effect = [b"opener.preview1('cert','new-doc','tp')",
                                               b'{"res":"YQ=="}', b"encrypted",
                                               b"1234567890abcdef", b"ok", b'{"SUCC":"N"}']
        with patch("certificate_document.ReportCipher", return_value=cipher), \
                patch("certificate_document.render", return_value=({}, None)):
            _, metadata = issue(self.worker.school, self.preview, exe=Path("unused"), fonts=Path("unused"),
                                output_dir=output, system_ip=self.worker.system_ip)
        self.assertEqual(metadata["status"], "verification_pending")
        self.assertFalse(metadata["server_verification_success"])
        self.assertTrue(metadata["print_completion_sent"])
        self.assertEqual(json.loads((output / "metadata.json").read_text()), metadata)

    def test_new_unverified_result_is_not_published(self):
        with patch.object(self.worker, "native_basket", return_value={"new-doc": self.preview}), \
                patch("certificate_worker.issue", return_value=(Path("unused"), {
                    "print_completion_sent": True, "server_verification_success": False})) as common:
            result = self.worker.pdf({"document_id": "new-doc"})
        self.assertEqual(result["status"], "verification_pending")
        self.assertNotIn("pdf_base64", result)
        common.assert_called_once()

    def test_free_issuance_polls_until_document_appears(self):
        with patch.object(self.worker, "native_basket", side_effect=[{}, {}, {}, {"new-doc": self.preview}]), \
                patch("certificate_worker.time.sleep"):
            result = self.worker.submit_free("scholarship", {})
        self.assertEqual(result["document_id"], "new-doc")
        self.worker.school.post.assert_called_once()

    def test_free_timeout_retains_pending_and_next_prepare_only_checks_list(self):
        self.worker.school.post.side_effect = TimeoutError()
        with patch.object(self.worker, "native_basket", return_value={}), patch("certificate_worker.time.sleep"):
            result = self.worker.submit_free("scholarship", {})
        self.assertEqual(result["status"], "issuance_pending")
        with patch.object(self.worker, "native_basket", return_value={"new-doc": self.preview}):
            recovered = self.worker.prepare({"kind": "scholarship"})
        self.assertEqual(recovered["document_id"], "new-doc")
        self.worker.school.post.assert_called_once()

    def test_ambiguous_free_results_do_not_choose_a_document(self):
        with patch.object(self.worker, "native_basket", side_effect=[{}, *[
                {"one": self.preview, "two": self.preview} for _ in range(4)]]), \
                patch("certificate_worker.time.sleep"):
            result = self.worker.submit_free("scholarship", {})
        self.assertEqual(result["status"], "issuance_pending")
        self.assertEqual(result["reason"], "ambiguous")
        self.assertNotIn("document_id", result)
        self.worker.school.post.assert_called_once()


if __name__ == "__main__":
    unittest.main()
