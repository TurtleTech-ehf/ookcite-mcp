#!/usr/bin/env python3
"""One plain-http mock for every host the new discovery tools talk to.

It stands in for OokCite, Crossref, DataCite, the DOI handle API, Unpaywall and
OpenAlex at once, with canned records. Nothing here touches a live host.
Run: demo/mock_upstream.py [port]   (prints the base URL on its first line)
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse

PLAIN = lambda doi: {"DOI": doi, "type": "journal-article"}

CROSSREF = {
    "10.5555/clean.2020": PLAIN("10.5555/clean.2020"),
    "10.5555/pub.2021": PLAIN("10.5555/pub.2021"),
    "10.5555/found.2018": PLAIN("10.5555/found.2018"),
    "10.5555/demo.a": PLAIN("10.5555/demo.a"),
    "10.5555/demo.b": PLAIN("10.5555/demo.b"),
    "10.5555/retracted.2019": {
        "DOI": "10.5555/retracted.2019",
        "update-to": [
            {"type": "retraction", "DOI": "10.5555/notice.2020", "updated": {"date-parts": [[2020, 6]]}}
        ],
    },
    "10.5555/demo.r": {
        "DOI": "10.5555/demo.r",
        "update-to": [
            {"type": "retraction", "DOI": "10.5555/demo.r.notice", "updated": {"date-parts": [[2022, 3]]}}
        ],
    },
    "10.5555/preprint.2021": {
        "DOI": "10.5555/preprint.2021",
        "type": "posted-content",
        "subtype": "preprint",
        "relation": {"is-preprint-of": [{"id-type": "doi", "id": "10.5555/pub.2021"}]},
    },
}

DATACITE = {
    "10.48550/arxiv.2201.00001": {
        "types": {"resourceTypeGeneral": "Preprint"},
        "relatedIdentifiers": [
            {"relationType": "IsPreprintOf", "relatedIdentifierType": "DOI", "relatedIdentifier": "10.5555/pub.2022"}
        ],
    }
}

UNPAYWALL = {
    "10.5555/demo.oa.2024": {
        "doi": "10.5555/demo.oa.2024",
        "title": "A Mock Study of Open Access Links",
        "journal_name": "Journal of Mock Results",
        "year": 2024,
        "is_oa": True,
        "oa_status": "gold",
        "best_oa_location": {
            "url": "https://journals.example.org/article/demo.oa.2024",
            "url_for_pdf": "https://journals.example.org/article/demo.oa.2024.pdf",
            "url_for_landing_page": "https://journals.example.org/article/demo.oa.2024",
            "host_type": "publisher",
            "version": "publishedVersion",
            "license": "cc-by",
        },
        "oa_locations": [
            {
                "url_for_pdf": "https://journals.example.org/article/demo.oa.2024.pdf",
                "url_for_landing_page": "https://journals.example.org/article/demo.oa.2024",
                "host_type": "publisher",
                "version": "publishedVersion",
                "license": "cc-by",
            },
            {
                "url_for_pdf": None,
                "url_for_landing_page": "https://repo.example.edu/handle/demo/2024",
                "host_type": "repository",
                "repository_institution": "Example University",
                "version": "publishedVersion",
                "license": None,
            },
        ],
    },
    "10.5555/demo.closed.2023": {
        "doi": "10.5555/demo.closed.2023",
        "title": "A Mock Paywalled Study",
        "journal_name": "Journal of Mock Results",
        "year": 2023,
        "is_oa": False,
        "oa_status": "closed",
        "best_oa_location": None,
        "oa_locations": [],
    },
}


def work(wid, title, year, doi, cited):
    return {
        "id": f"https://openalex.org/{wid}",
        "doi": f"https://doi.org/{doi}" if doi else None,
        "display_name": title,
        "publication_year": year,
        "cited_by_count": cited,
        "authorships": [{"author": {"display_name": n}} for n in ("Ada Example", "Ben Sample", "Chi Mock", "Dana Demo")],
        "primary_location": {"source": {"display_name": "Journal of Mock Results"}},
    }


SEED = {
    "id": "https://openalex.org/W100",
    "doi": "https://doi.org/10.5555/demo.seed",
    "display_name": "A Mock Seed Paper",
    "publication_year": 2021,
    "cited_by_count": 120,
    "related_works": [f"https://openalex.org/W20{n}" for n in (1, 2, 3)],
    "referenced_works": [f"https://openalex.org/W30{n}" for n in (1, 2)],
}
WORKS = {
    "W201": work("W201", "First Related Study", 2020, "10.5555/demo.rel1", 41),
    "W202": work("W202", "Second Related Study", 2019, "10.5555/demo.rel2", 8),
    "W203": work("W203", "Third Related Study", 2018, "10.5555/demo.rel3", 3),
    "W301": work("W301", "Foundations of Mock Shear", 2005, "10.5555/demo.ref1", 900),
    "W302": work("W302", "A Reference Without Superlatives", 2011, "10.5555/demo.ref2", 55),
}

TITLES = {
    "fast crystallisation": {
        "doi": "10.5555/found.2018",
        "title": "Fast crystallisation of sheared colloidal suspensions",
        "date": {"year": 2018},
        "authors": [{"family": "Smith"}, {"family": "Jones"}],
    }
}


def hit(title, doi, year, journal, score):
    return {"score": score, "metadata": {"title": title, "doi": doi, "journal": journal, "date": {"year": year}}}


CORPUS = {
    "total": 5,
    "offset": 0,
    "matches": [
        hit("Quiescent suspensions of hard spheres", "10.5555/demo.b", 2015, "Soft Matter Demo", 90.0),
        hit("Faster crystal nucleation in sheared colloidal glasses", "10.5555/demo.r", 2021, "Journal of Mock Physics", 80.0),
        hit("Crystal nucleation in sheared colloidal glasses", "10.5555/demo.a", 2021, "Journal of Mock Physics", 40.0),
        hit("A review without an identifier", None, 2020, "Review Demo", 99.0),
    ],
}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, status, body=None, headers=None):
        payload = json.dumps(body).encode() if body is not None else b""
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        url = urlparse(self.path)
        path = unquote(url.path)
        query = parse_qs(url.query)
        if path.startswith("/v2/"):
            doi = path[4:].lower()
            if doi == "10.5555/demo.limited":
                return self.send(429, {"message": "Too many requests"}, {"retry-after": "120"})
            return self.send(200, UNPAYWALL[doi]) if doi in UNPAYWALL else self.send(404, {"message": "not found"})
        if path.startswith("/works/doi:"):
            return self.send(200, SEED) if path[11:] == "10.5555/demo.seed" else self.send(404)
        if path == "/works":
            flt = query.get("filter", [""])[0]
            if flt.startswith("openalex:"):
                ids = flt[9:].split("|")
                return self.send(200, {"meta": {"count": len(ids)}, "results": [WORKS[i] for i in ids if i in WORKS]})
            return self.send(200, {"meta": {"count": 0}, "results": []})
        if path.startswith("/works/"):
            message = CROSSREF.get(path[7:].lower())
            return self.send(200, {"status": "ok", "message": message}) if message else self.send(404)
        if path.startswith("/dois/"):
            attrs = DATACITE.get(path[6:].lower())
            return self.send(200, {"data": {"attributes": attrs}}) if attrs else self.send(404)
        if path.startswith("/handles/"):
            return self.send(200, {"responseCode": 1})
        if path == "/api/v1/search/enhanced":
            return self.send(200, CORPUS)
        self.send(404, {"message": f"no mock for GET {path}"})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length).decode()
        path = urlparse(self.path).path
        if path == "/api/v1/lookup/doi":
            return self.send(200, {"doi": json.loads(raw).get("doi"), "title": "A mock record"})
        if path == "/api/v1/resolve":
            for needle, paper in TITLES.items():
                if needle in raw.lower():
                    return self.send(200, {"paper": paper})
            return self.send(404, {"message": "no match"})
        if path == "/api/v1/reverse":
            return self.send(200, [])
        self.send(404, {"message": f"no mock for POST {path}"})


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 0), Handler)
    print(f"http://127.0.0.1:{server.server_address[1]}", flush=True)
    server.serve_forever()
