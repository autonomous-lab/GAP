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
  for (const view of ['access', 'activity']) {
    const link = nav.querySelector(`[data-view="${view}"]`);
    if (link) advanced.append(link);
  }
  nav.after(advanced);
  viewNames.overview = 'Your workspace';
  viewNames.machines = 'Your MicroVMs';
  viewNames.billing = 'Billing';

  const projectHome = document.createElement('div');
  projectHome.id = 'projectHome';
  byId('projectsSection').append(projectHome);
  const projectNumber = p => [...allProjects].sort((a, b) => a.id.localeCompare(b.id)).findIndex(x => x.id === p.id) + 1;
  const projectName = p => `Project ${projectNumber(p)}`;
  const projectRows = p => fleetRows.filter(row => row.p.id === p.id && row.vm);
  const projectRoute = () => /^#project\/(prj_[0-9a-f]{24})$/.exec(location.hash)?.[1];

  function renderProjects() {
    const current = projectRoute();
    projectHome.replaceChildren();
    if (current) {
      const p = allProjects.find(x => x.id === current);
      if (!p) {
        const missing = document.createElement('p');
        missing.textContent = 'This project is not available in your account.';
        projectHome.append(missing);
        return;
      }
      const back = document.createElement('a');
      back.href = '#projects';
      back.className = 'project-back';
      back.textContent = '← All projects';
      const heading = document.createElement('div');
      heading.className = 'project-detail-head';
      const title = document.createElement('h3');
      title.textContent = projectName(p);
      const summary = document.createElement('p');
      summary.textContent = `${p.node} · ${projectRows(p).length} MicroVM${projectRows(p).length === 1 ? '' : 's'} · ${p.role} access`;
      heading.append(title, summary);
      const actions = document.createElement('div');
      actions.className = 'project-detail-actions';
      if (p.role !== 'viewer') {
        const create = document.createElement('button');
        create.className = 'primary';
        create.textContent = 'Create a MicroVM';
        create.onclick = run(() => placementDialog(p.id));
        actions.append(create);
      }
      const machines = document.createElement('div');
      machines.className = 'project-machine-list';
      const rows = projectRows(p);
      if (!rows.length) {
        const empty = document.createElement('p');
        empty.textContent = 'No MicroVM in this project yet. Create one here, or return to your machines.';
        machines.append(empty);
      }
      for (const row of rows) {
        const button = document.createElement('button');
        button.className = 'project-machine';
        button.textContent = `MicroVM ${row.vm.vm_id.slice(-8)} · ${row.vm.state} · ${row.p.node}  →`;
        button.onclick = run(async () => {
          if (p.role !== 'viewer') return openMachine(row.p, row.vm.vm_id, false);
          await details(row);
          let info = machines.querySelector('.vm-card-details');
          if (!info) { info = document.createElement('pre'); info.className = 'vm-card-details'; machines.append(info); }
          info.textContent = byId('fleetDetails').textContent;
        });
        machines.append(button);
      }
      const technical = document.createElement('details');
      technical.className = 'project-technical';
      const technicalSummary = document.createElement('summary');
      technicalSummary.textContent = 'Technical details and API access';
      const id = document.createElement('code');
      id.textContent = p.id;
      const apiButton = document.createElement('button');
      apiButton.textContent = 'Generate a temporary API token';
      const tokenOutput = document.createElement('p');
      tokenOutput.className = 'project-token-output';
      apiButton.onclick = run(async () => {
        const result = await api('project-token', {project_id:p.id});
        tokenOutput.textContent = `Expires in 2 minutes: ${result.token}`;
        setTimeout(() => { tokenOutput.textContent = ''; }, 120000);
      });
      technical.append(technicalSummary, id, apiButton, tokenOutput);
      projectHome.append(back, heading, actions, machines, technical);
      return;
    }
    const intro = document.createElement('p');
    intro.className = 'project-intro';
    intro.textContent = 'Projects group machines and permissions. Open one to see its MicroVMs; API tokens are only for integrations.';
    const grid = document.createElement('div');
    grid.className = 'project-grid';
    for (const p of allProjects) {
      const item = document.createElement('article');
      item.className = 'project-card';
      const title = document.createElement('h3');
      title.textContent = projectName(p);
      const description = document.createElement('p');
      description.textContent = `${p.node} · ${projectRows(p).length} MicroVM${projectRows(p).length === 1 ? '' : 's'} · ${p.role} access`;
      const open = document.createElement('a');
      open.href = '#project/' + p.id;
      open.textContent = 'Open project →';
      item.append(title, description, open);
      grid.append(item);
    }
    projectHome.append(intro, grid);
  }

  const originalPlacementDialog = placementDialog;
  placementDialog = function (preferredProject) {
    originalPlacementDialog();
    const dialog = document.querySelector('dialog:last-of-type');
    const description = dialog.querySelector('.section-intro');
    description.textContent = 'Start with 0.5 vCPU, 512 MiB RAM and 8 GiB disk. Customize the size or SSH key only if you need to.';
    const labels = [...dialog.querySelectorAll('label')];
    const projectLabel = labels.find(label => label.firstChild?.textContent === 'Project');
    if (projectLabel) {
      const select = projectLabel.querySelector('select');
      for (const option of select.options) {
        const p = allProjects.find(x => x.id === option.value);
        if (p) option.textContent = `${projectName(p)} · ${p.node}`;
      }
      if (preferredProject && [...select.options].some(option => option.value === preferredProject)) select.value = preferredProject;
      projectLabel.firstChild.textContent = 'Add to project';
    }
    dialog.querySelector('details summary').textContent = 'Advanced: size and SSH key';
  };

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
    sub.textContent = projectName(row.p) + ' · ' + row.p.node;
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
    const rows = fleetRows.filter(row => row.vm && (!byId('fleetNode').value || row.p.node === byId('fleetNode').value)
      && (!byId('fleetProject').value || row.p.id === byId('fleetProject').value));
    cards.replaceChildren();
    if (!rows.length) {
      const empty = document.createElement('p');
      empty.className = 'vm-empty';
      empty.textContent = allProjects.length
        ? 'No MicroVM yet. Create one to start; existing empty projects are listed under Projects.'
        : 'A project is needed before your first MicroVM. Create one to get started.';
      cards.append(empty);
      return;
    }
    for (const row of rows) cards.append(card(row));
  }
  const originalDrawFleet = drawFleet;
  drawFleet = function () { originalDrawFleet(); renderCards(); renderProjects(); };

  const accessPanel = byId('memberSection');
  accessPanel.querySelector('p').textContent = 'Your own access is automatic. Add another agent only when you want to collaborate on a project.';
  const accessAdvanced = document.createElement('details');
  accessAdvanced.className = 'access-advanced';
  const accessSummary = document.createElement('summary');
  accessSummary.textContent = 'Manage agents and permissions';
  accessAdvanced.append(accessSummary, byId('memberForm'), byId('members'));
  accessPanel.append(accessAdvanced);

  const originalLoadMigrations = loadMigrations;
  loadMigrations = async function () {
    await originalLoadMigrations();
    migrationTitle.textContent = 'Recent MicroVM moves';
    for (const [index, move] of migrationJobs.entries()) {
      const line = migrationList.children[index];
      if (!line?.firstChild) continue;
      const machine = `MicroVM ${move.vm_id.slice(-8)}`;
      const path = `${move.source_node} → ${move.target_node}`;
      const progress = move.job.total && move.job.status === 'running'
        ? ` · ${Math.round(100 * move.job.received / move.job.total)}% transferred` : '';
      line.firstChild.textContent = move.job.status === 'failed'
        ? `${machine} · move failed (${path})${move.job.error ? ': ' + move.job.error : ''} `
        : move.phase === 'committed' ? `${machine} · moved ${path} `
          : `${machine} · moving ${path} · ${move.job.status}${progress} `;
    }
  };

  const originalShowView = showView;
  showView = function () {
    originalShowView();
    const view = projectRoute() ? 'projects' : location.hash.slice(1) || 'overview';
    if (view === 'overview') {
      fleet.style.display = '';
      byId('projectsSection').style.display = 'none';
      document.querySelector('.account-hero p').textContent = 'Your machines and spending, without the infrastructure maze.';
    } else if (view === 'machines') {
      document.querySelector('.account-hero p').textContent = 'All your machines, across every host.';
    } else if (view === 'billing') {
      document.querySelector('.account-hero p').textContent = 'One shared wallet. Project spending limits are managed inside each VM.';
    }
    advanced.open = ['access', 'activity'].includes(view);
    if (projectRoute()) {
      for (const id of ['overviewStats','billingSection','fleetSection','memberSection','activitySection']) byId(id).style.display = 'none';
      byId('projectsSection').style.display = '';
      const active = allProjects.find(p => p.id === projectRoute());
      document.querySelector('.account-hero h1').textContent = active ? projectName(active) : 'Project';
      document.querySelector('.account-hero .eyebrow').textContent = 'Workspace / Projects / Project';
      document.querySelectorAll('[data-view]').forEach(link => link.removeAttribute('aria-current'));
      document.querySelector('.console-links [data-view="projects"]')?.setAttribute('aria-current', 'page');
    }
    renderProjects();
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
