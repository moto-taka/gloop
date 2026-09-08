import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const html = readFileSync(new URL('../src/workspace.html', import.meta.url), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
// Execute the actual functions without wiring browser events or starting boot.
const functions = script.slice(0, script.indexOf("$('taskForm').onsubmit="));

function harness() {
  const elements = new Map();
  const element = id => {
    if (!elements.has(id)) elements.set(id, {
      value: '', hidden: false, disabled: false, textContent: '', innerHTML: '',
      checked: false, required: false, children: [],
      classList: { toggle() {} }, focus() {},
      replaceChildren(...items) { this.children = items; },
      append(item) { this.children.push(item); },
      add(item) { this.children.push(item); },
      querySelectorAll() { return []; },
    });
    return elements.get(id);
  };
  const context = vm.createContext({
    document: { getElementById: element, createElement: () => element(Symbol()), hidden: false },
    location: { hash: '#test-token' },
    Option: function Option(text, value) { this.textContent = text; this.value = value; },
    setTimeout: () => 1, clearTimeout() {}, Date, AbortController,
  });
  vm.runInContext(functions, context);
  element('timeout').value = '1800';
  element('maxCalls').value = '3';
  return { context, element, run: code => vm.runInContext(code, context) };
}

test('saved workflows require an explicit selection, including index zero', () => {
  const h = harness();
  h.run("workflows = [{graph:{spec:{nodes:[]}},issues:[]}]; mode='workflow'");
  h.run('updatePreview()');
  assert.equal(h.element('submit').disabled, true);
  h.element('workflow').value = '0';
  h.run('updatePreview()');
  assert.equal(h.element('submit').disabled, false);
  h.run("workflows[0].issues=[{severity:'error'}]; updatePreview()");
  assert.equal(h.element('submit').disabled, true);
});

test('a late task response cannot replace a newer selection or the composer', async () => {
  const h = harness();
  let resolve;
  h.context.loadTask = () => new Promise(done => { resolve = done; });
  h.run('api = loadTask');
  const request = h.run("selectTask('old-task')");
  h.run('newTask()');
  resolve({ id: 'old-task', finished: true });
  await request;
  assert.equal(h.run('selected'), null);
  assert.equal(h.run('detail'), null);
  assert.equal(h.element('result').hidden, true);
  assert.equal(h.element('composer').hidden, false);
});

test('provider output and task titles are rendered as text, not HTML', () => {
  const h = harness();
  h.context.task = {
    id: 'safe', status: 'completed', finished: true, run_dir: '/project/.gloop/runs/safe',
    job: { created_at_ms: 1, request: { goal: '<img src=x onerror=alert(1)>' } },
    nodes: { work: { status: 'succeeded', profile: '<script>bad()</script>', output: '<img src=x onerror=alert(1)>', error: '<b>error</b>' } },
    events: [],
  };
  h.run('renderDetail(task)');
  assert.equal(h.element('resultTitle').textContent, '<img src=x onerror=alert(1)>');
  const markup = h.element('nodeResults').innerHTML;
  assert.ok(!markup.includes('<script>'));
  assert.ok(!markup.includes('<img'));
  assert.ok(markup.includes('&lt;img'));
  assert.ok(markup.includes('&lt;b&gt;error&lt;/b&gt;'));
});

test('after three connection failures polling stops until an explicit refresh', async () => {
  const h = harness();
  h.context.fail = async () => { throw new Error('offline'); };
  h.run("api=fail; selected='running'; detail={finished:false,job:{created_at_ms:Date.now(),request:{timeout_seconds:1800}}}");
  await h.run('poll()');
  await h.run('poll()');
  await h.run('poll()');
  assert.equal(h.run('pollFailures'), 3);
  assert.equal(h.element('connection').textContent, h.run("tr('disconnected')"));
});

test('direct tasks use one call and cross-model confirmation uses exactly two', () => {
  const h = harness();
  h.element('goal').value = 'Do the work';
  h.element('profile').value = 'codex';
  assert.equal(h.run('collectRequest().max_calls'), 1);
  h.element('secondOpinion').checked = true;
  h.element('reviewProfile').value = 'claude';
  const request = h.run('collectRequest()');
  assert.equal(request.max_calls, 2);
  assert.equal(request.review_profile, 'claude');
  assert.equal(request.after, null);
});
