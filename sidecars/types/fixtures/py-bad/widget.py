class Widget:
    id: str
    name: str


class WidgetList:
    items: list


class Error:
    message: str


# A deliberate syntax error: py_compile must fail on the line below.
def broken(:
    pass
