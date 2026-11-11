// junit.mjs — minimal JUnit XML parser for the flake probe.
//
// Returns one entry per <testcase>: { name, classname, suite, failed,
// skipped }. `suite` is the name of the enclosing <testsuite> (a test
// file path from node --test, a module path from pytest). `failed` is
// true when the testcase holds a <failure> or <error> child. `skipped`
// is true when it holds a <skipped> child.
//
// No dependencies. The scanner is stack-based, so the <testsuites>
// wrapper pytest emits does not confuse it.
export function decodeXml(text) {
  return text
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&apos;/g, "'")
    .replace(/&amp;/g, "&");
}

function parseAttrs(tag) {
  const attrs = {};
  const re = /([\w:.-]+)="([^"]*)"/g;
  let m;
  while ((m = re.exec(tag)) !== null) {
    attrs[m[1]] = decodeXml(m[2]);
  }
  return attrs;
}

export function parseJUnit(xml) {
  const cases = [];
  const tagRe = /<\/?(?:testsuites|testsuite|testcase)\b[^>]*>/g;
  const stack = [];
  let m;
  while ((m = tagRe.exec(xml)) !== null) {
    const tag = m[0];
    const kind = tag.match(/^<\/?([\w:]+)/)[1];
    if (tag.startsWith("</")) {
      stack.pop();
      continue;
    }
    const attrs = parseAttrs(tag);
    const selfClose = tag.endsWith("/>");
    if (kind === "testcase") {
      const parent = stack[stack.length - 1];
      let inner = "";
      if (!selfClose) {
        const close = xml.indexOf("</testcase>", tagRe.lastIndex);
        if (close === -1) {
          inner = xml.slice(tagRe.lastIndex);
          tagRe.lastIndex = xml.length;
        } else {
          inner = xml.slice(tagRe.lastIndex, close);
          tagRe.lastIndex = close + "</testcase>".length;
        }
      }
      cases.push({
        name: attrs.name || "(unnamed)",
        classname: attrs.classname || "",
        suite: parent ? parent.name || "" : "",
        failed: /<(?:failure|error)\b/.test(inner),
        skipped: /<skipped\b/.test(inner),
      });
    } else if (!selfClose) {
      stack.push({ kind, name: attrs.name || "" });
    }
  }
  return cases;
}
