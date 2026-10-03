(() => {
  const byId = id => document.getElementById(id);
  const account = byId('account');
  const fleet = byId('fleetSection');
  const cards = document.createElement('div');
  cards.id = 'vmCards';
  cards.setAttribute('aria-live', 'polite');
  fleet.insertBefore(cards, fleet.querySelector('div[style*="overflow"]'));
  byId('overviewStats').after(fleet);
  fleet.querySelector('h2').textContent = 'Your MicroVMs';
  fleet.querySelector('p').textContent = 'Open a machine to connect, publish an app, or manage spending.';
  byId('placeVM').textContent = 'Create a MicroVM';

  const advanced = document.createElement('details');
  advanced.className = 'console-advanced';
  const summary = document.createElement('summary');
  summary.textContent = 'Advanced';
  advanced.append(summary);
  const nav = document.querySelector('.console-links');
  for (const view of ['projects', 'access', 'activity']) {
    const link = nav.querySelector(`[data-view="${view}"]`);
    if (link) advanced.append(link);
  }
  nav.after(advanced);
  viewNames.overview = 'Your workspace';
  viewNames.machines = 'Your MicroVMs';
  viewNames.billing = 'Billing';

  function card(row) {
    const item = document.createElement('article');
    item.className = 'vm-card' + (row.error ? ' vm-card-error' : '');
    const head = document.createElement('div');
    head.className = 'vm-card-head';
    const icon = document.createElement('span');
    icon.className = 'vm-card-icon';
    icon.setAttribute('aria-hidden', 'true');
    icon.textContent = '▣';
    const title = document.createElement('div');
    const name = document.createElement('h3');
    name.textContent = row.vm ? 'MicroVM ' + row.vm.vm_id.slice(-8) : 'New MicroVM';
    const sub = document.createElement('div');
    sub.className = 'vm-card-sub';
    sub.textContent = row.p.id + ' · ' + row.p.node;
    title.append(name, sub);
    head.append(icon, title);
    const state = document.createElement('span');
    state.className = 'state-pill';
    state.dataset.state = row.error ? 'error' : row.vm?.state || 'empty';
    state.textContent = row.error ? 'Host unavailable' : row.migration ? 'Migrating' : row.vm?.state || 'Ready to create';
    const description = document.createElement('p');
    description.className = 'vm-card-summary';
    description.textContent = row.error ? 'We could not load this project. Retry the inventory.' : row.vm
      ? `${row.vm.vcpus} vCPU · ${row.vm.memory_mib} MiB RAM · ${row.vm.disk_gib} GiB disk`
      : 'Choose resources and start your first workload.';
    const actions = document.createElement('div');
    actions.className = 'vm-card-actions';
    const button = document.createElement('button');
    button.type = 'button';
    const readOnly = row.p.role === 'viewer';
    button.textContent = row.error ? 'Retry' : readOnly ? 'View connection' : row.vm ? 'Open MicroVM →' : 'Create MicroVM →';
    button.disabled = readOnly && !row.vm;
    button.onclick = run(async () => {
      if (row.error) return loadFleet();
      if (!readOnly) return openMachine(row.p, row.vm?.vm_id, !row.vm);
      await details(row);
      let info = item.querySelector('.vm-card-details');
      if (!info) { info = document.createElement('pre'); info.className = 'vm-card-details'; item.append(info); }
      info.textContent = byId('fleetDetails').textContent;
    });
    actions.append(button);
    item.append(head, state, description, actions);
    return item;
  }

  function renderCards() {
    const create = byId('placeVM');
    create.textContent = allProjects.length ? 'Create a MicroVM' : 'Create your first project';
    create.onclick = allProjects.length ? run(() => placementDialog()) : () => { location.assign('/signup'); };
    const rows = fleetRows.filter(row => (!byId('fleetNode').value || row.p.node === byId('fleetNode').value)
      && (!byId('fleetProject').value || row.p.id === byId('fleetProject').value));
    cards.replaceChildren();
    if (!rows.length) {
      const empty = document.createElement('p');
      empty.className = 'vm-empty';
      empty.textContent = allProjects.length
        ? 'No MicroVM on this host yet. Create one to start.'
        : 'A project is needed before your first MicroVM. Create one to get started.';
      cards.append(empty);
      return;
    }
    for (const row of rows) cards.append(card(row));
  }
  const originalDrawFleet = drawFleet;
  drawFleet = function () { originalDrawFleet(); renderCards(); };

  const originalShowView = showView;
  showView = function () {
    originalShowView();
    const view = location.hash.slice(1) || 'overview';
    if (view === 'overview') {
      fleet.style.display = '';
      byId('projectsSection').style.display = 'none';
      document.querySelector('.account-hero p').textContent = 'Your machines and spending, without the infrastructure maze.';
    } else if (view === 'machines') {
      document.querySelector('.account-hero p').textContent = 'All your machines, across every host.';
    } else if (view === 'billing') {
      document.querySelector('.account-hero p').textContent = 'One shared wallet. Project spending limits are managed inside each VM.';
    }
    advanced.open = ['projects', 'access', 'activity'].includes(view);
  };
  window.addEventListener('hashchange', () => showView());

  const billingHelp = document.createElement('div');
  billingHelp.id = 'billingHelp';
  byId('billingSection').append(billingHelp);
  function updateBilling() {
    const balance = Number.parseFloat(byId('balance').textContent.replace(/,/g, ''));
    billingHelp.dataset.blocked = String(Number.isFinite(balance) && balance <= 0);
    billingHelp.replaceChildren();
    const text = document.createElement('span');
    text.textContent = balance <= 0
      ? 'Your wallet has no available credits. A paused VM will keep its disk during the retention period. Ask your GAP operator to add credits before the deadline. '
      : 'This balance is shared by your projects. A VM can also pause when its project spending budget is reached. ';
    const link = document.createElement('a');
    link.href = '#machines';
    link.textContent = 'Review your MicroVMs →';
    billingHelp.append(text, link);
  }
  new MutationObserver(updateBilling).observe(byId('balance'), {childList: true, characterData: true, subtree: true});
  new MutationObserver(() => {
    document.body.classList.toggle('console-signed-in', !account.hidden);
    if (!account.hidden) { renderCards(); showView(); }
  }).observe(account, {attributes: true, attributeFilter: ['hidden']});
  document.body.classList.toggle('console-signed-in', !account.hidden);
  updateBilling();
  showView();
})();
