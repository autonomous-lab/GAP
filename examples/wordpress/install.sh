#!/bin/sh
set -eu
# Do not enable shell tracing: the environment contains application secrets.
if ! wp core is-installed >/dev/null 2>&1; then
    printf '%s\n' "$WP_ADMIN_PASSWORD" | wp core install \
        --url="$WP_PUBLIC_URL" --title="$WP_TITLE" \
        --admin_user="$WP_ADMIN_USER" --admin_email="$WP_ADMIN_EMAIL" \
        --skip-email --prompt=admin_password
fi
wp core is-installed
# Pretty permalinks with a gateway that strips the external path prefix.
wp option update permalink_structure '/%postname%/'
cat > .htaccess <<'RULES'
<IfModule mod_rewrite.c>
RewriteEngine On
RewriteRule .* - [E=HTTP_AUTHORIZATION:%{HTTP:Authorization}]
RewriteBase /
RewriteRule ^index\.php$ - [L]
RewriteCond %{REQUEST_FILENAME} !-f
RewriteCond %{REQUEST_FILENAME} !-d
RewriteRule . /index.php [L]
</IfModule>
RULES
touch /tmp/wordpress-installed
# A healthy bootstrap service lets Compose --wait cover installation itself.
exec sleep infinity
