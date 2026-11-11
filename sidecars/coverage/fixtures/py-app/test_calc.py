import unittest

import calc


class TestCalc(unittest.TestCase):
    def test_add(self):
        self.assertEqual(calc.add(2, 2), 4)

    def test_div(self):
        self.assertEqual(calc.div(6, 2), 3)
        with self.assertRaises(ValueError):
            calc.div(1, 0)


if __name__ == "__main__":
    unittest.main()
