"""과금 없이 주문 결합·승인 중복 방지·발급 확인을 검증한다."""

import concurrent.futures
import io
import json
from pathlib import Path
import tempfile
import threading
import unittest
from email.message import Message
from urllib.request import Request

from certificate_checkout import APPROVAL_PATH, Basket, Checkout, School, SchoolOnlyRedirect

NATIVE_APPROVAL_PATH = "/wm1.0/jsp/pay/lg/smart/newcertres.jsp"


class FakeSchool:
    def __init__(self, *, timeout=False, pdf=True, issued=True, native=False):
        self.calls = []
        self.lock = threading.Lock()
        self.timeout = timeout
        self.pdf = pdf
        self.issued = issued
        self.native = native

    def post(self, path, fields):
        with self.lock:
            self.calls.append((path, dict(fields)))
        if path == APPROVAL_PATH:
            if self.timeout:
                raise TimeoutError()
            return b"<html>approval response is not proof of payment</html>"
        if "CERTPDFDOWN" in path:
            return b"%PDF-1.7\nmock" if self.pdf else b"<html>session expired</html>"
        if "BASKETLIST" in path and self.issued:
            if self.native:
                return ("""<form><table><tr><td>\n"""
                        """<a href="javascript:preview('재학증명서','owned-document','null','1','0','Y','1','Y')">"""
                        """preview</a></td></tr></table></form>""").encode()
            return b"""<form><input name=GIWANNO value=021163><input name=COMMAND value=old>
                <table><tr><td><input name=MINNO0 value=owned-document>
                <a href="javascript:certdown('0','B');">download</a></td></tr></table></form>"""
        return b"<form><p>no issued document</p></form>"

    def approvals(self, path=APPROVAL_PATH):
        return [fields for called, fields in self.calls if called == path]


class FakeIssuer:
    """승인 뒤 발급 파이프라인 대역. 실제 문서 수신·출력 통지는 하지 않는다."""

    def __init__(self, directory, *, content=b"%PDF-1.7\nrendered", error=None):
        self.directory = directory
        self.content = content
        self.error = error
        self.calls = []
        self.lock = threading.Lock()

    def __call__(self, school, numbers):
        with self.lock:
            self.calls.append(set(numbers))
        if self.error:
            raise self.error
        path = self.directory / f"rendered-{len(self.calls)}.pdf"
        path.write_bytes(self.content)
        return path, {"print_completion_sent": True, "server_verification_success": True,
                      "document_number": "owned-document"}


class CheckoutTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.config = {"public_base": "https://checkout.example", "fields": {
            "LGD_AMOUNT": "3000", "LGD_OID": "owned-order", "LGD_MID": "merchant",
            "LGD_HASHDATA": "signed-by-school", "MINNOS": "owned-document@",
            "GIWANNO": "021163", "RECEIVER": "original-receiver"}}

    def flow(self, school=None, *, live=True):
        school = school or FakeSchool()
        path = Path(self.temp.name) / "state.json"
        checkout = Checkout(self.config, path, school, allow_approval=live)
        checkout.open()
        return checkout, school

    def success(self, **extra):
        result = {"LGD_RESPCODE": ["0000"], "LGD_PAYKEY": ["authenticated-key"]}
        result.update(extra)
        return result

    def test_cancel_never_approves(self):
        checkout, school = self.flow()
        self.assertTrue(checkout.receive({"LGD_RESPCODE": ["S053"]}))
        self.assertEqual(school.calls, [])
        self.assertFalse(checkout.snapshot()["approval_attempted"])

    def test_success_uses_server_order_and_only_callback_paykey(self):
        checkout, school = self.flow()
        checkout.receive(self.success(RECEIVER=["injected"], LGD_HASHDATA=["injected"]))
        approved = school.approvals()[0]
        self.assertEqual(approved["LGD_OID"], "owned-order")
        self.assertEqual(approved["LGD_AMOUNT"], "3000")
        self.assertEqual(approved["RECEIVER"], "original-receiver")
        self.assertEqual(approved["LGD_HASHDATA"], "signed-by-school")
        self.assertEqual(approved["LGD_PAYKEY"], "authenticated-key")
        self.assertTrue(checkout.snapshot()["pdf_ready"])
        self.assertFalse(checkout.snapshot()["paid_verified"])
        self.assertEqual(checkout.state_path.with_suffix(".approval-response.bin").stat().st_mode & 0o777, 0o600)
        callback = json.loads(checkout.state_path.with_suffix(".callback.json").read_text())
        self.assertEqual(callback["LGD_PAYKEY"], ["authenticated-key"])

    def test_parallel_duplicate_callbacks_approve_once(self):
        checkout, school = self.flow()
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(lambda _: checkout.receive(self.success()), range(8)))
        self.assertEqual(sum(results), 1)
        self.assertEqual(len(school.approvals()), 1)

    def test_approval_timeout_only_rechecks_basket(self):
        checkout, school = self.flow(FakeSchool(timeout=True, issued=False))
        checkout.receive(self.success())
        checkout.receive(self.success())
        checkout.reconcile()
        self.assertEqual(len(school.approvals()), 1)
        self.assertEqual(checkout.snapshot()["status"], "approval_unknown")

    def test_issued_document_recovers_after_approval_timeout(self):
        checkout, school = self.flow(FakeSchool(timeout=True, issued=False))
        checkout.receive(self.success())
        school.issued = True
        checkout.reconcile()
        self.assertTrue(checkout.snapshot()["pdf_ready"])
        self.assertEqual(len(school.approvals()), 1)

    def test_order_or_amount_mismatch_rejected(self):
        checkout, school = self.flow()
        for name, value in [("LGD_OID", "other-order"), ("LGD_AMOUNT", "1"), ("LGD_MID", "other")]:
            self.assertFalse(checkout.receive(self.success(**{name: [value]})))
        self.assertEqual(school.calls, [])

    def test_success_without_paykey_rejected(self):
        checkout, school = self.flow()
        self.assertFalse(checkout.receive({"LGD_RESPCODE": ["0000"]}))
        self.assertEqual(school.calls, [])

    def test_duplicate_callback_fields_rejected(self):
        checkout, school = self.flow()
        self.assertFalse(checkout.receive(self.success(LGD_AMOUNT=["3000", "1"])))
        self.assertEqual(school.calls, [])

    def test_trial_mode_does_not_approve(self):
        checkout, school = self.flow(live=False)
        checkout.receive(self.success())
        self.assertEqual(school.calls, [])
        self.assertEqual(checkout.snapshot()["status"], "authenticated_approval_disabled")

    def test_html_response_is_not_a_pdf(self):
        checkout, _ = self.flow(FakeSchool(pdf=False))
        checkout.receive(self.success())
        self.assertFalse(checkout.snapshot()["pdf_ready"])
        self.assertFalse(checkout.pdf_path.exists())

    def test_http_200_approval_is_not_completion(self):
        checkout, _ = self.flow(FakeSchool(issued=False))
        checkout.receive(self.success())
        self.assertFalse(checkout.snapshot()["pdf_ready"])
        self.assertFalse(checkout.snapshot()["paid_verified"])

    def test_restart_cannot_overwrite_attempted_order(self):
        checkout, _ = self.flow(FakeSchool(issued=False))
        checkout.receive(self.success())
        with self.assertRaises(FileExistsError):
            Checkout(self.config, checkout.state_path, FakeSchool(), allow_approval=True)
        self.assertTrue(json.loads(checkout.state_path.read_text())["approval_attempted"])
        self.assertNotIn("authenticated-key", checkout.state_path.read_text())

    def test_nested_layout_cannot_select_unrelated_document(self):
        basket = Basket("""<table><tr><td><input name=MINNOS value=owned-document>
            <table><tr><td><input name=MINNO0 value=other-document>
            <a href="javascript:certdown('0','B');">download</a></td></tr></table>
            </td></tr></table>""")
        self.assertIsNone(basket.download_index({"owned-document"}))

    def test_raw_http_exchange_is_saved_before_response_handling(self):
        trace = Path(self.temp.name) / "trace"
        school = School("JSESSIONID=test", trace)
        response = io.BytesIO(b'<form action="/next-step">important follow-up</form>')
        response.url = "https://certi.ssu.ac.kr/result"
        response.status = 200
        response.headers = Message()
        response.headers["Set-Cookie"] = "JSESSIONID=changed"

        class Opener:
            def open(self, request, timeout):
                self.assert_request = request
                return response

        school.opener = Opener()
        body = school.post(APPROVAL_PATH, {"LGD_PAYKEY": "private-key"})
        self.assertEqual((trace / "0001-request.bin").read_bytes(), b"LGD_PAYKEY=private-key")
        self.assertEqual((trace / "0002-response.bin").read_bytes(), body)
        self.assertIn("JSESSIONID=changed", (trace / "0002-response.json").read_text())
        for path in trace.iterdir():
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_redirect_response_and_auto_submit_form_are_preserved(self):
        trace = Path(self.temp.name) / "redirect-trace"
        school = School("JSESSIONID=test", trace)
        handler = SchoolOnlyRedirect(school.record)
        request = Request("https://certi.ssu.ac.kr/approve", b"original-order")
        headers = Message()
        headers["Location"] = "https://certi.ssu.ac.kr/next"
        body = b'<form action="/register"><input name=receipt value=approved></form>'
        redirected = handler.redirect_request(request, io.BytesIO(body), 302, "Found", headers,
                                             "https://certi.ssu.ac.kr/next")
        self.assertEqual(redirected.full_url, "https://certi.ssu.ac.kr/next")
        self.assertEqual((trace / "0001-redirect.bin").read_bytes(), body)

    def test_general_print_approval_path_is_explicit(self):
        self.config["approval_path"] = "/wm1.0/jsp/pay/lg/smart/newcertres.jsp"
        checkout, school = self.flow()
        checkout.receive(self.success())
        self.assertEqual(school.calls[0][0], self.config["approval_path"])

    def test_unknown_approval_path_is_rejected(self):
        self.config["approval_path"] = "https://example.com/approve"
        with self.assertRaises(ValueError):
            self.flow()

    def test_phone_is_required_before_opening_general_checkout(self):
        self.config["requires_phone"] = True
        checkout = Checkout(self.config, Path(self.temp.name) / "state.json", FakeSchool())
        with self.assertRaises(ValueError):
            checkout.open()
        self.assertEqual(checkout.snapshot()["status"], "ready")
        fields = checkout.open("01012345678")
        self.assertEqual(fields["BUYERTEL"], "01012345678")
        self.assertEqual(checkout.fields["BUYERTEL"], "01012345678")

    def native(self, *, issue=None, school=None):
        self.config["fields"].update(SERVICE_TYPE="INTERNET", MINNOS="owned-document@",
                                     LGD_AMOUNT="1000")
        self.config["approval_path"] = NATIVE_APPROVAL_PATH
        school = school or FakeSchool(native=True)
        path = Path(self.temp.name) / "native.json"
        checkout = Checkout(self.config, path, school, allow_approval=True, issue=issue)
        checkout.open()
        return checkout, school

    def test_paid_native_order_continues_to_rendered_pdf(self):
        issuer = FakeIssuer(Path(self.temp.name))
        checkout, school = self.native(issue=issuer)
        checkout.receive(self.success())
        state = checkout.snapshot()
        self.assertEqual(state["status"], "issued_pdf_ready")
        self.assertTrue(state["pdf_ready"])
        self.assertTrue(state["print_completion_sent"])
        self.assertEqual(checkout.pdf_path.read_bytes(), b"%PDF-1.7\nrendered")
        self.assertEqual(issuer.calls, [{"owned-document"}])
        self.assertEqual(len(school.approvals(NATIVE_APPROVAL_PATH)), 1)
        # 티켓 주소로 공개되는 상태에는 문서 번호나 확인번호를 담지 않는다.
        self.assertNotIn("owned-document", json.dumps(state))
        self.assertFalse(state["paid_verified"])

    def test_issue_runs_once_for_repeated_callbacks_and_refresh(self):
        issuer = FakeIssuer(Path(self.temp.name))
        checkout, _ = self.native(issue=issuer)
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(lambda _: checkout.receive(self.success()), range(8)))
        for _ in range(3):
            checkout.reconcile()
        self.assertEqual(len(issuer.calls), 1)

    def test_failed_issue_does_not_retry_or_reapprove(self):
        issuer = FakeIssuer(Path(self.temp.name), error=TimeoutError())
        checkout, school = self.native(issue=issuer)
        checkout.receive(self.success())
        self.assertEqual(checkout.snapshot()["status"], "issue_unknown")
        self.assertFalse(checkout.pdf_path.exists())
        checkout.reconcile()
        self.assertEqual(len(issuer.calls), 1)
        self.assertEqual(len(school.approvals(NATIVE_APPROVAL_PATH)), 1)

    def test_rendered_output_must_be_a_pdf(self):
        issuer = FakeIssuer(Path(self.temp.name), content=b"<html>login</html>")
        checkout, _ = self.native(issue=issuer)
        checkout.receive(self.success())
        self.assertEqual(checkout.snapshot()["status"], "issued_document_format_unverified")
        self.assertFalse(checkout.snapshot()["pdf_ready"])
        self.assertFalse(checkout.pdf_path.exists())

    def test_native_order_without_renderer_stops_at_basket(self):
        checkout, _ = self.native()
        checkout.receive(self.success())
        self.assertEqual(checkout.snapshot()["status"], "issued_native_document_ready")
        self.assertFalse(checkout.snapshot()["pdf_ready"])

    def test_failed_original_verification_does_not_publish_pdf(self):
        issuer = FakeIssuer(Path(self.temp.name))
        def unverified(school, numbers):
            path, metadata = issuer(school, numbers)
            metadata["server_verification_success"] = False
            return path, metadata
        checkout, school = self.native(issue=unverified)
        checkout.receive(self.success())
        self.assertEqual(checkout.snapshot()["status"], "issued_document_verification_pending")
        self.assertFalse(checkout.snapshot()["pdf_ready"])
        self.assertFalse(checkout.pdf_path.exists())
        checkout.reconcile()
        self.assertEqual(len(issuer.calls), 1)
        self.assertEqual(len(school.approvals(NATIVE_APPROVAL_PATH)), 1)

    def test_document_number_prefix_cannot_match_other_order(self):
        basket = Basket("""<tr><td><a href="javascript:preview('cert','owned-document-other','null','1','0','Y','1','Y')">preview</a></td></tr>""")
        self.assertFalse(basket.has_native_document({"owned-document"}))

    def test_previews_skip_rows_without_print_counts(self):
        basket = Basket("""<table>
            <tr><td><a href="javascript:certdown('0','B');">pdf</a></td></tr>
            <tr><td><a href="javascript:preview('x','doc','null','n','0','Y','1','Y')">bad</a></td></tr>
            <tr><td><a href="javascript:reportx.preview('재학증명서','doc-2','null','2','1','Y','1','Y')">ok</a></td></tr>
            </table>""")
        self.assertEqual(basket.previews(), [{"job": "재학증명서", "minno": "doc-2", "count": 2,
                                              "printed": 1, "korean": "Y", "printgb": "1",
                                              "printable": "Y"}])

    def test_native_preview_is_separate_from_pdf(self):
        basket = Basket("""<table><tr><td><a href="javascript:preview('cert','owned-document','null','1','0','Y','1','Y')">preview</a></td></tr></table>""")
        self.assertTrue(basket.has_native_document({"owned-document"}))
        self.assertFalse(basket.has_native_document({"other-document"}))
        self.assertIsNone(basket.download_index({"owned-document"}))


if __name__ == "__main__":
    unittest.main()
