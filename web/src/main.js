import { mount } from 'svelte'
import './lib/press.css'
import App from './App.svelte'

export default mount(App, { target: document.getElementById('app') })
