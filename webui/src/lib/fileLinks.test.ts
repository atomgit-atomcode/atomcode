import { test } from 'node:test';
import assert from 'node:assert/strict';
import { localFilePath } from './fileLinks.ts';

test('local links decode once, preserve host paths and drop URL fragments', () => {
  const cases = [
    ['file:///Users/theo/%E5%AE%A3%E4%BC%A0%20%E5%9B%BE.png', '/Users/theo/宣传 图.png'],
    ['file://localhost/tmp/result.png', '/tmp/result.png'],
    ['file:///C:/Users/theo/result.png', 'C:/Users/theo/result.png'],
    ['C:\\Users\\theo\\result.png', 'C:\\Users\\theo\\result.png'],
    ['./gen_share_card.py#L10', './gen_share_card.py'],
    ['/tmp/a%23b%3Fc.txt', '/tmp/a#b?c.txt'],
    ['report%2520.txt', 'report%20.txt'],
    ['../output.png?download=1', '../output.png'],
  ];
  for (const [href, path] of cases) assert.equal(localFilePath(href), path);
});

test('external URLs, anchors, unsafe schemes and network shares never become local requests', () => {
  for (const href of ['https://example.com/file.png', 'http://example.com', 'mailto:a@b.com', 'tel:123', '#heading', '?view=1',
    '//example.com/file', '\\\\host\\share', 'file://remote/share/file', 'javascript:alert(1)', 'data:text/html,hi',
    'sandbox:/mnt/data/result.png', 'blob:https://example.com/id', '%2f%2fhost/share', 'java%73cript:alert(1)',
    'bad%00.png', 'bad\n.png', 'bad%FF', '']) assert.equal(localFilePath(href), null, href);
});

test('file preview Markdown resolves relative links beside the document', () => {
  assert.equal(localFilePath('../image.png', 'docs/notes/readme.md'), 'docs/notes/../image.png');
  assert.equal(localFilePath('/output.png', 'docs/readme.md'), '/output.png');
  assert.equal(localFilePath('C:/output.png', 'docs/readme.md'), 'C:/output.png');
});
