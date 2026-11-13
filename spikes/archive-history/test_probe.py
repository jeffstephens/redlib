import unittest
from probe import merge, normalize, validate

class ProbeTests(unittest.TestCase):
    def test_dedup_preserves_sources_and_newest_snapshot(self):
        a = normalize({"id":"abc", "author":"Example", "created_utc":100, "retrieved_on":101}, "a", "comments", "example")
        b = normalize({"id":"abc", "author":"example", "created_utc":100, "retrieved_on":102, "body":"[deleted]"}, "b", "comments", "example")
        result = merge([a,b])
        self.assertEqual(len(result),1)
        self.assertEqual(result[0]["sources"],["a","b"])
        self.assertEqual(result[0]["content_marker"],"[deleted]")

    def test_post_and_comment_ids_are_distinct_and_content_is_not_emitted(self):
        row={"id":"abc", "author":"example", "created_utc":100, "body":"not emitted", "selftext":"not emitted"}
        result=merge([normalize(row,"a",kind,"example") for kind in ["posts","comments"]])
        self.assertEqual(len(result),2)
        self.assertTrue(all("body" not in r and "selftext" not in r for r in result))
        self.assertIsNone(result[0]["nsfw"])

    def test_rejects_wrong_author_and_invalid_identity(self):
        self.assertIsNone(normalize({"id":"abc","author":"other","created_utc":100},"a","posts","example"))
        self.assertIsNone(normalize({"id":"../bad","author":"example","created_utc":100},"a","posts","example"))
        for name,limit in [("../bad",3),("example",0),("example",1000)]:
            with self.assertRaises(ValueError): validate(name,limit)

if __name__ == "__main__":unittest.main()
