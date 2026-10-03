(() => {
  const byId = id => document.getElementById(id);
  const workspace = byId('workspace');
  const connect = byId('connect');
  const form = byId('connect-form');
  const entry = document.createElement('div');
  entry.className = 'vm-account-entry';
  const entryText = document.createElement('p');
  entryText.textContent = 'Sign in once to see and manage all your MicroVMs. No project ID or access token needed.';
  const entryLink = document.createElement('a');
  entryLink.href = '/account#machines';
  entryLink.textContent = 'Open your dashboard →';
  entry.append(entryText, entryLink);
  const advanced = document.createElement('details');
  advanced.className = 'vm-token-access';
  const summary = document.createElement('summary');
  summary.textContent = 'Use a project access token (advanced)';
  advanced.append(summary, form);
  connect.append(entry, advanced);
  byId('connection-title').firstChild.textContent = 'Your MicroVM workspace';
  byId('connection-title').querySelector('span').textContent = 'Use your GAP account to open a machine, or connect with a project token below.';

  const tabs = document.createElement('nav');
  tabs.id = 'vmTabs';
  tabs.className = 'vm-tabs';
  tabs.setAttribute('aria-label', 'MicroVM sections');
  const names = {overview:'Overview', connect:'Connect', network:'Network', usage:'Usage', settings:'Settings'};
  const buttons = {};
  for (const [key, label] of Object.entries(names)) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = label;
    button.dataset.tab = key;
    button.onclick = () => activate(key, true);
    buttons[key] = button;
    tabs.append(button);
  }
  workspace.prepend(tabs);

  const funding = document.createElement('div');
  funding.id = 'vmFundingNotice';
  funding.className = 'vm-funding';
  funding.setAttribute('role', 'alert');
  funding.hidden = true;
  tabs.after(funding);

  const quick = document.createElement('section');
  quick.className = 'panel vm-quick';
  quick.dataset.vmSection = 'overview';
  const title = document.createElement('h2');
  title.textContent = 'Connect & publish';
  const description = document.createElement('p');
  description.textContent = 'The essentials for getting back to work.';
  const actions = document.createElement('div');
  actions.className = 'vm-quick-actions';
  const terminal = document.createElement('button');
  terminal.type = 'button';
  terminal.textContent = 'Open terminal';
  terminal.onclick = () => { activate('connect', true); byId('terminal-open').click(); };
  const ssh = document.createElement('button');
  ssh.type = 'button';
  ssh.textContent = 'Copy SSH command';
  ssh.onclick = () => byId('ssh-copy').click();
  const preview = document.createElement('a');
  preview.textContent = 'Open app preview ↗';
  preview.target = '_blank';
  preview.rel = 'noopener noreferrer';
  const note = document.createElement('p');
  note.className = 'vm-quick-note';
  note.textContent = 'Your app URL appears here as soon as HTTPS routing is enabled.';
  actions.append(terminal, ssh, preview);
  quick.append(title, description, actions, note);
  byId('alerts').after(quick);

  const children = [...workspace.children];
  const pricing = workspace.lastElementChild;
  for (const element of children) {
    if (element === tabs || element === funding || element === quick) continue;
    let group = 'overview';
    if (element.id === 'ssh-panel' || element.id === 'terminal-panel') group = 'connect';
    else if (element.id === 'network-panel') group = 'network';
    else if (element.id === 'vm-measurements' || element.classList.contains('usage') || element === pricing) group = 'usage';
    else if (element.classList.contains('columns') || element.id === 'delete-panel') group = 'settings';
    element.dataset.vmSection = group;
  }

  function activate(group, writeHash = false) {
    if (!Object.hasOwn(names, group)) group = 'overview';
    for (const element of workspace.querySelectorAll('[data-vm-section]')) {
      if (element.dataset.vmSection === group) element.dataset.active = 'true';
      else delete element.dataset.active;
    }
    for (const [key, button] of Object.entries(buttons)) {
      button.setAttribute('aria-selected', String(key === group));
      button.tabIndex = key === group ? 0 : -1;
    }
    if (writeHash) history.replaceState(null, '', location.pathname + location.search + '#vm-' + group);
  }
  const initial = location.hash.startsWith('#vm-') ? location.hash.slice(4) : 'overview';
  activate(initial);
  window.addEventListener('hashchange', () => activate(location.hash.slice(4)));
  for (const [id, group] of [['new-vm','overview'],['open-create','overview'],['open-delete','settings']]) {
    byId(id).addEventListener('click', () => queueMicrotask(() => activate(group, true)));
  }

  function syncQuick() {
    const hasVM = !!vm?.vm_id && vm.state !== 'destroyed';
    terminal.disabled = !hasVM || byId('terminal-open').hidden;
    ssh.disabled = !hasVM || byId('ssh-command-box').hidden || !byId('ssh-command').textContent.trim();
    const published = hasVM && !byId('network-url').hidden && !!byId('network-url').href;
    preview.href = published ? byId('network-url').href : '#vm-network';
    preview.setAttribute('aria-disabled', String(!published));
    note.textContent = published ? 'Your application is reachable through its HTTPS URL.'
      : 'Your app URL appears here as soon as HTTPS routing is enabled.';
  }
  for (const element of [byId('ssh-command'), byId('ssh-command-box'), byId('network-url')]) {
    new MutationObserver(syncQuick).observe(element, {childList:true, subtree:true, attributes:true});
  }

  function fundingNotice(account) {
    const alerts = account?.alerts || [];
    const budget = alerts.includes('budget_exhausted');
    const wallet = alerts.includes('credits_exhausted') || account?.fleet?.funding_status === 'exhausted';
    const blocked = budget || wallet;
    funding.hidden = !blocked;
    funding.replaceChildren();
    if (!blocked) return;
    const heading = document.createElement('strong');
    heading.textContent = budget ? 'Project spending limit reached' : 'Account credits exhausted';
    const body = document.createElement('span');
    body.textContent = budget ? 'This VM cannot resume until the project budget is raised or removed.'
      : 'This VM cannot resume until credits become available. Stored data may continue to incur charges.';
    funding.append(heading, body);
    if (budget) {
      const button = document.createElement('button');
      button.type = 'button';
      button.textContent = 'Review project budget →';
      button.onclick = () => {activate('settings', true); byId('budget').scrollIntoView({block:'center',behavior:'smooth'});};
      funding.append(button);
    } else {
      const link = document.createElement('a');
      link.href = '/account#billing';
      link.textContent = 'Open billing dashboard →';
      funding.append(link);
    }
  }
  const originalRender = render;
  render = function (machine, account) {
    originalRender(machine, account);
    fundingNotice(account);
    syncQuick();
  };
  const originalRefresh = refresh;
  refresh = async function (...args) { await originalRefresh(...args); syncQuick(); };
  new MutationObserver(() => document.body.classList.toggle('vm-connected', !workspace.hidden))
    .observe(workspace, {attributes:true, attributeFilter:['hidden']});
  document.body.classList.toggle('vm-connected', !workspace.hidden);
  syncQuick();
})();
