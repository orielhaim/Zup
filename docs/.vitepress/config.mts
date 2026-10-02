import { defineConfig } from 'vitepress'

const github = 'https://github.com/orielhaim/Zup'

export default defineConfig({
  title: 'Zup',
  description: 'Build, sign and publish desktop installers from one manifest. Cross-platform, with Windows shipping today.',
  cleanUrls: true,
  lastUpdated: true,
  ignoreDeadLinks: false,

  head: [['meta', { name: 'theme-color', content: '#111111' }]],

  themeConfig: {
    logo: '/zup.svg',
    siteTitle: 'Zup',

    nav: [
      { text: 'Guide', link: '/guide/' },
      { text: 'Presets', link: '/presets/' },
      { text: 'Plugins', link: '/plugins/' },
      { text: 'Ship', link: '/ship/' },
      { text: 'Reference', link: '/reference/' },
      { text: 'Help', link: '/help/' },
      { text: 'GitHub', link: github },
    ],

    // Sidebar links are joined against the section's `base`, so they are not
    // absolute: `link: ''` is falsy and renders inert text with no anchor, and
    // `link: '/guide/'` would resolve to `/guide/guide/`. `'/'` joins back to the
    // base.
    //
    // Every sidebar stays inside its own section. A link that leaves it makes
    // VitePress swap the whole sidebar on arrival, so the entries the reader was
    // using to get there disappear while they are still reading the page.
    sidebar: {
      '/guide/': {
        base: '/guide/',
        items: [
          { text: 'Guide', link: '/' },
          {
            text: 'Getting started',
            collapsed: false,
            items: [
              { text: 'Quick start', link: 'quickstart' },
              { text: 'Project', link: 'project' },
            ],
          },
          {
            text: 'The manifest',
            collapsed: false,
            items: [
              { text: 'Platforms and targets', link: 'platforms' },
              { text: 'Frontends', link: 'frontends' },
              { text: 'Install scope and paths', link: 'install' },
              { text: 'Payload files', link: 'payload' },
              { text: 'Components', link: 'components' },
              { text: 'Selection and conditions', link: 'selection' },
              { text: 'System integration', link: 'integration' },
              { text: 'Prerequisites', link: 'prerequisites' },
            ],
          },
          {
            text: 'Building',
            collapsed: false,
            items: [
              { text: 'Check and preview', link: 'preview' },
              { text: 'Build', link: 'build' },
            ],
          },
          {
            text: 'After installation',
            collapsed: false,
            items: [{ text: 'Application lifecycle', link: 'lifecycle' }],
          },
        ],
      },

      '/presets/': {
        base: '/presets/',
        items: [
          { text: 'Presets', link: '/' },
          {
            text: 'Using presets',
            collapsed: false,
            items: [{ text: 'Use a preset', link: 'use' }],
          },
          {
            text: 'Authoring',
            collapsed: false,
            items: [
              { text: 'Create a preset', link: 'create' },
              { text: 'Development loop', link: 'develop' },
              { text: 'Settings and assets', link: 'settings-assets' },
              { text: 'State and actions', link: 'state-actions' },
            ],
          },
          {
            text: 'Packaging',
            collapsed: false,
            items: [{ text: 'Package a preset', link: 'package' }],
          },
        ],
      },

      '/plugins/': {
        base: '/plugins/',
        items: [
          { text: 'Plugins', link: '/' },
          {
            text: 'Configuration',
            collapsed: false,
            items: [{ text: 'Configure a plugin', link: 'configure' }],
          },
          {
            text: 'Authoring',
            collapsed: false,
            items: [
              { text: 'Create a plugin', link: 'create' },
              { text: 'Planner context', link: 'context' },
              { text: 'Resources', link: 'resources' },
              { text: 'Constraints', link: 'constraints' },
            ],
          },
        ],
      },

      '/ship/': {
        base: '/ship/',
        items: [
          { text: 'Ship', link: '/' },
          {
            text: 'Release contents',
            collapsed: false,
            items: [
              { text: 'Artifacts', link: 'artifacts' },
              { text: 'Signing', link: 'signing' },
            ],
          },
          {
            text: 'Publishing',
            collapsed: false,
            items: [
              { text: 'GitHub Releases', link: 'github' },
              { text: 'Release CI', link: 'ci' },
              { text: 'Static hosting', link: 'web' },
            ],
          },
          {
            text: 'Updates',
            collapsed: false,
            items: [{ text: 'Updates', link: 'updates' }],
          },
        ],
      },

      '/reference/': {
        base: '/reference/',
        items: [
          { text: 'Reference', link: '/' },
          {
            text: 'Contracts',
            collapsed: false,
            items: [
              { text: 'zup.toml', link: 'manifest' },
              { text: 'CLI', link: 'cli' },
            ],
          },
          {
            text: 'APIs',
            collapsed: false,
            items: [
              { text: 'Preset API', link: 'preset-api' },
              { text: 'Plugin API', link: 'plugin-api' },
            ],
          },
          {
            text: 'Automation',
            collapsed: false,
            items: [{ text: 'Automation output', link: 'automation' }],
          },
        ],
      },

      '/help/': {
        base: '/help/',
        items: [
          { text: 'Help', link: '/' },
          {
            text: 'Diagnosing problems',
            collapsed: false,
            items: [{ text: 'Troubleshooting', link: 'troubleshooting' }],
          },
        ],
      },
    },

    search: { provider: 'local' },
    outline: { level: [2, 3] },
    socialLinks: [{ icon: 'github', link: github }],
    editLink: {
      pattern: `${github}/edit/master/docs/:path`,
      text: 'Edit this page',
    },
    docFooter: {
      prev: 'Previous',
      next: 'Next',
    },
  },
})