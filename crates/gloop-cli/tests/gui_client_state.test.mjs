import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', 'src');
const html = readFileSync(join(root, 'gui.html'), 'utf8');

function extractBalancedFunction(source, startIndex) {
  const open = source.indexOf('{', startIndex);
  assert.ok(open >= 0, 'missing function body');
  let depth = 0;
  for (let index = open; index < source.length; index += 1) {
    const character = source[index];
    if (character === '{') depth += 1;
    else if (character === '}') {
      depth -= 1;
      if (depth === 0) {
        return source.slice(startIndex, index + 1);
      }
    }
  }
  throw new Error('unbalanced function braces');
}

function extractFunctions(names) {
  const script = html.match(/<script>([\s\S]*)<\/script>/)[1];
  return names
    .map((name) => {
      const asyncMarker = `async function ${name}`;
      const syncMarker = `function ${name}`;
      const asyncStart = script.indexOf(asyncMarker);
      const syncStart = script.indexOf(syncMarker);
      const start = asyncStart >= 0 ? asyncStart : syncStart;
      assert.ok(start >= 0, `missing function ${name}`);
      return extractBalancedFunction(script, start);
    })
    .join('\n');
}

function createMockElement(id) {
  const optionList = [];
  const handlers = {};
  const element = {
    id,
    value: '',
    textContent: '',
    innerHTML: '',
    className: '',
    classList: { toggle() {} },
    options: optionList,
    disabled: false,
    append(option) {
      optionList.push(option);
    },
    setAttribute() {},
    addEventListener(type, handler) {
      handlers[type] = handler;
    },
    get onchange() {
      return this._onchange;
    },
    set onchange(handler) {
      this._onchange = handler;
    },
    get oninput() {
      return this._oninput;
    },
    set oninput(handler) {
      this._oninput = handler;
    },
    get onclick() {
      return this._onclick;
    },
    set onclick(handler) {
      this._onclick = handler;
    },
    dispatch(type) {
      const handler = this[`on${type}`] || handlers[type];
      if (handler) handler();
    },
  };
  Object.defineProperty(element, 'children', {
    get() {
      return optionList;
    },
  });
  return element;
}

function createHarness() {
  const elements = new Map();
  const apiCalls = [];
  const document = {
    getElementById(id) {
      if (!elements.has(id)) {
        elements.set(id, createMockElement(id));
      }
      return elements.get(id);
    },
    createElement() {
      return createMockElement('dynamic');
    },
    querySelector() {
      return null;
    },
    documentElement: { lang: 'en' },
    addEventListener() {},
  };

  const context = {
    document,
    window: {
      addEventListener() {},
      confirm() {
        return true;
      },
    },
    location: { hash: '#token' },
    fetch(url, options) {
      return context.recordApi(url, options);
    },
    setTimeout,
    clearTimeout,
    console,
    T: {
      en: {
        modelHelp: 'model help',
        modelHelpUnsupported: 'unsupported',
        modelHelpFailed: 'failed ({reason})',
        modelHelpCustom: 'custom',
        defaultModel: 'Use tool default',
        none: 'Choose automatically',
        runtimeDefault: 'runtime default',
        runtimeDefaultOption: 'Runtime default: {name}',
        profileKinds: { command: 'local CLI', openai: 'OpenAI API', anthropic: 'Anthropic API' },
        saving: 'Saving…',
        saved: 'Saved',
        unsaved: 'Unsaved changes',
        unsavedClose: 'Discard changes?',
        selected: 'Step',
        kindNames: { agent: 'AI processing', command: 'Command execution', verify: 'Result verification', gate: 'Approval checkpoint', reduce: 'Consolidate results', synthesize: 'Produce final result', loop: 'Repeat workflow', subgraph: 'Nested workflow' },
        kindDescriptions: { agent: 'Give instructions to an AI and use its response.', verify: 'Run a check; a non-zero result stops the workflow.' },
      },
    },
    lang: 'en',
    state: null,
    graph: null,
    profiles: [],
    runtimeModels: [],
    selectedId: null,
    dirty: false,
    saving: false,
    draftRevision: 0,
    savedRevision: 0,
    positions: new Map(),
    scale: 1,
    pan: { x: 0, y: 0 },
    connectingFrom: null,
    selectedEdge: null,
    promptOriginal: null,
    dragging: null,
    KINDS: ['agent'],
    INFO: { agent: { icon: 'AI' } },
    tr(key) {
      return this.T[this.lang][key] ?? key;
    },
    kind(node) {
      return node?.kind || 'agent';
    },
    nodeList() {
      return this.graph?.spec?.nodes || [];
    },
    selectedNode() {
      return this.nodeList().find((node) => node.id === this.selectedId) || null;
    },
    setDirty() {
      this.dirty = true;
      this.draftRevision += 1;
    },
    setSaved() {
      this.dirty = false;
      this.savedRevision = this.draftRevision;
    },
    notice() {},
    renderCanvas() {},
    render() {},
    kindName(node) {
      return this.tr('kindNames')[this.kind(node)] || this.kind(node);
    },
    renderProfiles(current) {
      const select = this.$('profile');
      select.innerHTML = '';
      const none = this.document.createElement();
      none.value = '';
      none.textContent = this.tr('none');
      select.append(none);
      this.profiles
        .filter((profile) => profile.enabled)
        .forEach((profile) => {
          const option = this.document.createElement();
          option.value = profile.name;
          option.textContent = profile.name;
          select.append(option);
        });
      select.value = current || '';
    },
    renderBefore() {},
    renderEdgeList() {},
    renderTypeChoices() {},
    renderInspector() {},
    validateDraft() {
      return true;
    },
    startPan() {},
    addNode() {},
    zoom() {},
    fitCanvas() {},
    renderText() {},
    removeNode() {},
    syncBefore() {},
    changeKind() {},
    fresh() {
      return {};
    },
    copyCommon() {},
    $(id) {
      return document.getElementById(id);
    },
    apiCalls,
    saveApiHook: null,
    async recordApi(path, options = {}) {
      if (this.saveApiHook) {
        return this.saveApiHook(path, options);
      }
      apiCalls.push({ path, options });
      return { success: true };
    },
  };

  const source = `function tr(key){return T[lang][key]??key}
function kind(node){return node?.kind||'agent'}
function nodeList(){return graph?.spec?.nodes||[]}
function selectedNode(){return nodeList().find(node=>node.id===selectedId)||null}
function $(id){return document.getElementById(id)}
function setDirty(){dirty=true;draftRevision+=1}
function setSaved(){dirty=false;savedRevision=draftRevision}
function notice(){}
function render(){}
async function recordApi(path,options){if(saveApiHook)return saveApiHook(path,options);apiCalls.push({path,options});return {success:true}}
async function api(path,options){return recordApi(path,options)}
${extractFunctions([
    'selectedProfile',
    'profileModels',
    'modelLabel',
    'catalogIncludes',
    'parseArgv',
    'quoteArg',
    'formatArgv',
    'renderModelNote',
    'renderModels',
    'profileLabel',
    'renderProfiles',
    'renderKindOptions',
    'rememberRuntimeModel',
    'applyNode',
    'friendlyName',
    'hasRequiredInput',
    'incompleteMessage',
    'validateDraft',
    'save',
    'closeEditor',
    'setupEvents',
  ])}`;

  vm.createContext(context);
  vm.runInContext(source, context);
  ['topName', 'name', 'goal', 'parallel', 'description', 'statusText', 'save', 'close', 'viewport', 'zoomIn', 'zoomOut', 'zoomReset', 'fit', 'lang', 'remove', 'before', 'kind', 'nodeName', 'prompt', 'modelChoice', 'modelAdvanced', 'profile', 'edgeFrom', 'edgeTo', 'edgeKind', 'addEdge', 'advancedHelp'].forEach((id) => {
    context.$(id);
  });
  context.$('goal').value = 'goal';
  context.$('parallel').value = '1';
  context.setupEvents();
  return context;
}

function agentNode(id, profile, model) {
  return {
    id,
    kind: 'agent',
    label: id,
    prompt: 'do work',
    profile,
    model,
    retry: { max_attempts: 1, backoff_seconds: 0, rebind_profiles: [] },
    workspace: { mode: 'current' },
    context: { include_dependencies: true, files: [], max_bytes: 262144 },
    requires: [],
    resources: [],
    continue_on_failure: false,
    output: { format: 'text', max_bytes: 1048576 },
    fan_out: 1,
  };
}

function catalogModel(id, label = id) {
  return { id, label };
}

test('custom model on node A does not leak into model-less node B', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'writer',
      enabled: true,
      models: [catalogModel('composer-2.5', 'Composer 2.5')],
      discovery: 'listed',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [
        agentNode('a', 'writer', 'claude-opus-4'),
        agentNode('b', 'writer'),
      ],
      edges: [],
      policies: {},
    },
  };

  harness.selectedId = 'a';
  harness.renderModels(harness.selectedNode());
  assert.equal(harness.$('modelAdvanced').value, 'claude-opus-4');

  harness.selectedId = 'b';
  harness.renderModels(harness.selectedNode());
  assert.equal(harness.$('modelAdvanced').value, '');
  assert.equal(harness.$('modelChoice').value, '');

  harness.applyNode();
  assert.equal(harness.selectedNode().model, undefined);
});

test('profile switch clears model for listed target', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'profile-a',
      enabled: true,
      models: [catalogModel('model-a')],
      discovery: 'listed',
    },
    {
      name: 'profile-b',
      enabled: true,
      models: [catalogModel('model-b')],
      discovery: 'listed',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a', 'profile-a', 'model-a')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.renderProfiles('profile-a');
  harness.renderModels(harness.selectedNode());
  harness.$('profile').value = 'profile-b';
  harness.$('profile').dispatch('change');

  assert.equal(harness.selectedNode().model, undefined);
  assert.equal(harness.$('modelAdvanced').value, '');
  assert.equal(harness.$('modelChoice').value, '');
});

test('profile switch clears model for failed discovery target', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'writer',
      enabled: true,
      models: [catalogModel('composer-2.5')],
      discovery: 'listed',
    },
    {
      name: 'broken',
      enabled: true,
      models: [],
      discovery: 'failed',
      discovery_error: 'timeout',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a', 'writer', 'custom-model')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.$('prompt').value = 'do work';
  harness.renderProfiles('writer');
  harness.renderModels(harness.selectedNode());
  harness.$('profile').value = 'broken';
  harness.$('profile').dispatch('change');

  assert.equal(harness.selectedNode().profile, 'broken');
  assert.equal(harness.selectedNode().model, undefined);
  assert.equal(harness.$('modelAdvanced').value, '');
});

test('profile switch clears model for unsupported discovery target', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'writer',
      enabled: true,
      models: [catalogModel('composer-2.5')],
      discovery: 'listed',
    },
    {
      name: 'openai',
      enabled: true,
      models: [catalogModel('gpt-5')],
      discovery: 'unsupported',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a', 'writer', 'custom-model')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.$('prompt').value = 'do work';
  harness.renderProfiles('writer');
  harness.renderModels(harness.selectedNode());
  harness.$('profile').value = 'openai';
  harness.$('profile').dispatch('change');

  assert.equal(harness.selectedNode().profile, 'openai');
  assert.equal(harness.selectedNode().model, undefined);
  assert.equal(harness.$('modelAdvanced').value, '');
});

test('re-selecting the same profile keeps the current model', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'writer',
      enabled: true,
      models: [catalogModel('composer-2.5')],
      discovery: 'listed',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a', 'writer', 'composer-2.5')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.renderProfiles('writer');
  harness.renderModels(harness.selectedNode());
  harness.$('profile').value = 'writer';
  harness.$('profile').dispatch('change');

  assert.equal(harness.selectedNode().model, 'composer-2.5');
  assert.equal(harness.$('modelChoice').value, 'composer-2.5');
});

test('manual custom model after profile switch saves normally', async () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'writer',
      enabled: true,
      models: [catalogModel('composer-2.5')],
      discovery: 'listed',
    },
    {
      name: 'openai',
      enabled: true,
      models: [catalogModel('gpt-5')],
      discovery: 'unsupported',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a', 'writer', 'custom-model')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.$('prompt').value = 'do work';
  harness.renderProfiles('writer');
  harness.renderModels(harness.selectedNode());
  harness.$('profile').value = 'openai';
  harness.$('profile').dispatch('change');
  harness.$('modelAdvanced').value = 'manual-model';
  harness.$('modelAdvanced').dispatch('input');
  await harness.save();

  assert.equal(harness.selectedNode().model, 'manual-model');
  assert.equal(harness.apiCalls.at(-1)?.path, '/api/save');
  const payload = JSON.parse(harness.apiCalls.at(-1).options.body);
  assert.equal(payload.graph.spec.nodes[0].model, 'manual-model');
});

test('choosing tool default clears a carried custom model', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'writer',
      enabled: true,
      models: [catalogModel('composer-2.5', 'Composer 2.5')],
      discovery: 'listed',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a', 'writer', 'custom-model')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.renderModels(harness.selectedNode());
  harness.$('modelChoice').value = '';
  harness.$('modelChoice').dispatch('change');

  assert.equal(harness.selectedNode().model, undefined);
  assert.equal(harness.$('modelAdvanced').value, '');
});

test('runtime default profile clears a carried custom model', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'writer',
      enabled: true,
      models: [catalogModel('composer-2.5', 'Composer 2.5')],
      discovery: 'listed',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a', 'writer', 'custom-model')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.runtimeModels = ['legacy-model'];
  const runtimeModelsBeforeSwitch = [...harness.runtimeModels];
  harness.renderProfiles('writer');
  harness.renderModels(harness.selectedNode());
  harness.$('profile').value = '';
  harness.$('profile').dispatch('change');

  assert.equal(harness.selectedNode().profile, undefined);
  assert.equal(harness.selectedNode().model, undefined);
  assert.equal(harness.$('modelAdvanced').value, '');
  assert.equal(harness.$('modelChoice').value, '');
  assert.deepEqual(harness.runtimeModels, runtimeModelsBeforeSwitch);
});

test('cursor catalog renders friendly labels while saving ids', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'cursor',
      enabled: true,
      models: [
        catalogModel('gpt-5.6-luna-xhigh', 'GPT-5.6 Luna 1M Extra High'),
      ],
      discovery: 'listed',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a', 'cursor')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.renderModels(harness.selectedNode());
  const option = harness.$('modelChoice').options.find(
    (entry) => entry.value === 'gpt-5.6-luna-xhigh',
  );
  assert.ok(option);
  assert.equal(option.textContent, 'GPT-5.6 Luna 1M Extra High (gpt-5.6-luna-xhigh)');
  harness.$('modelChoice').value = 'gpt-5.6-luna-xhigh';
  harness.$('modelChoice').dispatch('change');
  assert.equal(harness.selectedNode().model, 'gpt-5.6-luna-xhigh');
});

test('edit during an in-flight save keeps the draft dirty', async () => {
  const harness = createHarness();
  let releaseSave;
  harness.api = async () => {
    await new Promise((resolve) => {
      releaseSave = resolve;
    });
    return { success: true };
  };
  harness.graph = {
    metadata: { name: 'flow' },
    spec: { goal: 'goal', nodes: [], edges: [], policies: {} },
  };
  harness.setSaved();

  const pending = harness.save();
  for (let attempt = 0; attempt < 50 && !releaseSave; attempt += 1) {
    await new Promise((resolve) => {
      setImmediate(resolve);
    });
  }
  assert.ok(releaseSave, 'save request should be in flight');
  harness.setDirty();
  releaseSave();
  await pending;

  assert.equal(harness.dirty, true);
  assert.equal(harness.draftRevision, harness.savedRevision + 1);
});

test('immediate close during save sends no close request', async () => {
  const harness = createHarness();
  let releaseSave;
  harness.saveApiHook = async (path) => {
    if (path === '/api/save') {
      await new Promise((resolve) => {
        releaseSave = resolve;
      });
    }
    harness.apiCalls.push({ path, options: {} });
    return { success: true };
  };
  harness.graph = {
    metadata: { name: 'flow' },
    spec: { goal: 'goal', nodes: [], edges: [], policies: {} },
  };

  const pendingSave = harness.save();
  for (let attempt = 0; attempt < 50 && !releaseSave; attempt += 1) {
    await new Promise((resolve) => {
      setImmediate(resolve);
    });
  }
  await harness.closeEditor();
  releaseSave();
  await pendingSave;

  assert.deepEqual(
    harness.apiCalls.map((call) => call.path),
    ['/api/save'],
  );
});

test('close after save sends exactly one close after exactly one save', async () => {
  const harness = createHarness();
  harness.graph = {
    metadata: { name: 'flow' },
    spec: { goal: 'goal', nodes: [], edges: [], policies: {} },
  };

  await harness.save();
  await harness.closeEditor();

  assert.deepEqual(
    harness.apiCalls.map((call) => call.path),
    ['/api/save', '/api/close'],
  );
});

test('runtime default history appears in the model dropdown', () => {
  const harness = createHarness();
  harness.runtimeModels = ['legacy-model', 'another-model'];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a', undefined, 'legacy-model')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.renderProfiles('');
  harness.renderModels(harness.selectedNode());

  const ids = harness.$('modelChoice').options.map((option) => option.value);
  assert.deepEqual(ids, ['', 'legacy-model', 'another-model']);
  assert.equal(harness.$('modelChoice').value, 'legacy-model');
});

test('new AI step leaves tool choice explicit and exposes the recommended provider', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'backup',
      kind: 'command',
      enabled: true,
      runtime_default: false,
      models: [catalogModel('backup-model')],
      discovery: 'listed',
    },
    {
      name: 'primary',
      kind: 'openai',
      enabled: true,
      runtime_default: true,
      default_model: 'gpt-5',
      models: [catalogModel('gpt-5')],
      discovery: 'unsupported',
    },
  ];
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.renderProfiles(undefined);
  harness.renderModels(harness.selectedNode());

  assert.equal(harness.$('profile').value, '');
  assert.equal(harness.$('modelChoice').value, '');

  harness.$('profile').value = 'primary';
  harness.$('profile').dispatch('change');
  assert.equal(harness.$('profile').value, 'primary');
  assert.equal(harness.$('modelChoice').value, 'gpt-5');
  harness.applyNode();
  assert.equal(harness.selectedNode().profile, 'primary');
  assert.equal(harness.selectedNode().model, 'gpt-5');
});

test('technical node type selector explains internal values', () => {
  const harness = createHarness();
  harness.renderKindOptions('verify');

  assert.equal(harness.$('kind').value, 'verify');
  assert.equal(harness.$('kind').options[1].textContent, 'Command execution (command)');
  assert.equal(harness.$('kindDescription').textContent, 'Run a check; a non-zero result stops the workflow.');
});

test('profileModels returns empty catalog without an explicit profile', () => {
  const harness = createHarness();
  harness.profiles = [
    {
      name: 'openai',
      enabled: true,
      models: [catalogModel('gpt-5')],
      discovery: 'unsupported',
    },
  ];
  assert.equal(harness.profileModels(null).length, 0);
  harness.runtimeModels = ['legacy-model'];
  assert.deepEqual(
    harness.profileModels(null).map((model) => [model.id, model.label]),
    [['legacy-model', 'legacy-model']],
  );
  assert.deepEqual(
    harness.profileModels(harness.profiles[0]).map((model) => model.id),
    ['gpt-5'],
  );
});

test('command argv formatting round-trips shell-like values', () => {
  const harness = createHarness();
  const argv = ['bash', '-c', 'echo "a b"', "it's", 'a\nb', '', 'a\\b'];

  assert.deepEqual([...harness.parseArgv(harness.formatArgv(argv))], argv);
});

test('save blocks an incomplete new step before writing', async () => {
  const harness = createHarness();
  harness.graph = {
    metadata: { name: 'flow' },
    spec: {
      goal: 'goal',
      nodes: [agentNode('a')],
      edges: [],
      policies: {},
    },
  };
  harness.selectedId = 'a';
  harness.$('prompt').value = '';
  await harness.save();

  assert.deepEqual(harness.apiCalls, []);

  harness.$('prompt').value = 'do work';
  await harness.save();
  assert.deepEqual(harness.apiCalls.map((call) => call.path), ['/api/save']);
});
