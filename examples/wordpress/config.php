<?php
// GAP strips the /apps/.../ prefix. The fixed deployment URL is authoritative;
// do not derive the WordPress origin from a visitor-controlled Host header.
$public_url = rtrim(getenv('WP_PUBLIC_URL'), '/');
define('WP_HOME', $public_url);
define('WP_SITEURL', $public_url);
define('FORCE_SSL_ADMIN', true);
if (PHP_SAPI !== 'cli') {
    $_SERVER['HTTPS'] = 'on';
    $_SERVER['SERVER_PORT'] = 443;
    $prefix = rtrim(parse_url($public_url, PHP_URL_PATH) ?: '', '/');
    $_SERVER['REQUEST_URI'] = $prefix . '/' . ltrim($_SERVER['REQUEST_URI'] ?? '/', '/');
}
