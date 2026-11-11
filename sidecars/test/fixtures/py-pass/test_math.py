import unittest


def add(a, b):
    return a + b


class AddTest(unittest.TestCase):
    def test_add_small(self):
        self.assertEqual(add(2, 2), 4)

    def test_add_negative(self):
        self.assertEqual(add(-1, 1), 0)


if __name__ == "__main__":
    unittest.main()
